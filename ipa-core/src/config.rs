use std::{
    borrow::{Borrow, Cow},
    fmt::{Debug, Formatter},
    iter::zip,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hyper::{http::uri::Scheme, Uri};
use hyper_util::client::legacy::Builder;
use rustls_pemfile::Item;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::fs;

use crate::{
    error::BoxError,
    helpers::{HelperIdentity, TransportIdentity},
    hpke::{
        Deserializable as _, IpaPrivateKey, IpaPublicKey, KeyRegistry, PrivateKeyOnly,
        PublicKeyOnly, Serializable as _,
    },
    net::{ConnectionFlavor, Helper, Shard},
    sharding::ShardIndex,
};

pub type OwnedCertificate = CertificateDer<'static>;
pub type OwnedPrivateKey = PrivateKeyDer<'static>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    ParseError(#[from] config::ConfigError),
    #[error("Invalid uri: {0}")]
    InvalidUri(#[from] hyper::http::uri::InvalidUri),
    #[error("Invalid network size {0}")]
    InvalidNetworkSize(usize),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
    #[error("Missing shard ports for peers {0:?}")]
    MissingShardPorts(Vec<usize>),
}

/// Configuration describing either 3 peers in a Ring or N shard peers. In a non-sharded case a
/// single [`NetworkConfig`] represents the entire network. In a sharded case, each host should
/// have one Ring and one Sharded configuration to know how to reach its peers.
///
/// The most important thing this contains is discovery information for each of the participating
/// peers.
#[derive(Clone, Debug, Deserialize)]
pub struct NetworkConfig<F: ConnectionFlavor = Helper> {
    peers: Vec<PeerConfig>,

    /// HTTP client configuration.
    #[serde(default)]
    pub client: ClientConfig,

    /// The identities of the index-matching peers. Separating this from [`Self::peers`](field) so
    /// that parsing is easy to implement.
    #[serde(skip)]
    identities: Vec<F::Identity>,
}

impl<F: ConnectionFlavor> NetworkConfig<F> {
    /// # Panics
    /// If `PathAndQuery::from_str("")` fails
    #[must_use]
    pub fn override_scheme(self, scheme: &Scheme) -> Self {
        Self {
            peers: self
                .peers
                .into_iter()
                .map(|mut peer| {
                    let mut parts = peer.url.into_parts();
                    parts.scheme = Some(scheme.clone());
                    // `http::uri::Uri::from_parts()` requires that a URI have a path if it has a
                    // scheme. If the URI does not have a scheme, it is not required to have a path.
                    if parts.path_and_query.is_none() {
                        parts.path_and_query = Some("".parse().unwrap());
                    }
                    peer.url = Uri::try_from(parts).unwrap();
                    peer
                })
                .collect(),
            ..self
        }
    }

    #[must_use]
    pub fn vec_peers(&self) -> Vec<PeerConfig> {
        self.peers.clone()
    }

    #[must_use]
    pub fn get_peer(&self, i: usize) -> Option<&PeerConfig> {
        self.peers.get(i)
    }

    pub fn peers_iter(&self) -> std::slice::Iter<'_, PeerConfig> {
        self.peers.iter()
    }

    /// We currently require an exact match with the peer cert (i.e. we don't support verifying
    /// the certificate against a truststore and identifying the peer by the certificate
    /// subject). This could be changed if the need arises.
    #[must_use]
    pub fn identify_cert(&self, cert: Option<&CertificateDer>) -> Option<F::Identity> {
        let cert = cert?;
        for (id, p) in zip(self.identities.iter(), self.peers.iter()) {
            if p.certificate.as_ref() == Some(cert) {
                return Some(*id);
            }
        }
        // It might be nice to log something here. We could log the certificate base64?
        tracing::error!(
            "A client certificate was presented that does not match a known helper. Certificate: {}",
            BASE64.encode(cert),
        );
        None
    }
}

impl NetworkConfig<Shard> {
    /// # Panics
    /// In the unexpected case there are more than max usize shards.
    #[must_use]
    pub fn new_shards(peers: Vec<PeerConfig>, client: ClientConfig) -> Self {
        let identities = (0u32..peers.len().try_into().unwrap())
            .map(ShardIndex::from)
            .collect();
        Self {
            peers,
            client,
            identities,
        }
    }

    /// # Panics
    /// In the unexpected case there are more than max usize shards.
    #[must_use]
    pub fn shard_count(&self) -> ShardIndex {
        ShardIndex::try_from(self.peers.len()).unwrap()
    }
}

impl NetworkConfig<Helper> {
    /// Creates a new configuration for 3 MPC clients (ring) configuration.
    /// # Panics
    /// If the vector doesn't contain exactly 3 items.
    #[must_use]
    pub fn new_mpc(ring: Vec<PeerConfig>, client: ClientConfig) -> Self {
        assert_eq!(3, ring.len());
        Self {
            peers: ring,
            client,
            identities: HelperIdentity::make_three().to_vec(),
        }
    }

    /// Reads config from string. Expects config to be toml format.
    /// To read file, use `fs::read_to_string`
    ///
    /// # Errors
    /// if `input` is in an invalid format
    pub fn from_toml_str(input: &str) -> Result<Self, Error> {
        use config::{Config, File, FileFormat};

        let mut conf: Self = Config::builder()
            .add_source(File::from_str(input, FileFormat::Toml))
            .build()?
            .try_deserialize()?;

        conf.identities = HelperIdentity::make_three().to_vec();

        Ok(conf)
    }

    /// Clones the internal configs and returns them as an array.
    /// # Panics
    /// If the internal vector isn't of size 3.
    #[must_use]
    pub fn peers(&self) -> [PeerConfig; 3] {
        self.peers
            .clone()
            .try_into()
            .unwrap_or_else(|v: Vec<_>| panic!("Expected a Vec of length 3 but it was {}", v.len()))
    }
}

/// This struct is only used by [`parse_sharded_network_toml`] to parse the entire network.
/// Unlike [`NetworkConfig`], this one doesn't have identities.
#[derive(Clone, Debug, Deserialize)]
struct ShardedNetworkToml {
    pub peers: Vec<ShardedPeerConfigToml>,

    /// HTTP client configuration.
    #[serde(default)]
    pub client: ClientConfig,
}

impl ShardedNetworkToml {
    fn missing_shard_ports(&self) -> Vec<usize> {
        self.peers
            .iter()
            .enumerate()
            .filter_map(|(i, peer)| {
                if peer.shard_port.is_some() {
                    None
                } else {
                    Some(i)
                }
            })
            .collect()
    }
}

/// This struct is only used by [`parse_sharded_network_toml`] to generate [`PeerConfig`]. It
/// contains an optional `shard_port`.
#[derive(Clone, Debug, Deserialize)]
struct ShardedPeerConfigToml {
    #[serde(flatten)]
    pub config: PeerConfig,
    pub shard_port: Option<u16>,
}

impl ShardedPeerConfigToml {
    /// Clones the inner Peer.
    fn to_mpc_peer(&self) -> PeerConfig {
        self.config.clone()
    }

    /// Create a new Peer but its url using [`ShardedPeerConfigToml::shard_port`].
    fn to_shard_peer(&self) -> PeerConfig {
        let url = self.config.url.to_string();
        let new_url = format!(
            "{}{}",
            &url[..=url.find(':').unwrap()],
            self.shard_port.expect("Shard port should be set")
        );
        let mut shard_peer = self.config.clone();
        shard_peer.url = Uri::from_str(&new_url).expect("Problem creating uri with sharded port");
        shard_peer
    }
}

/// Parses a [`ShardedNetworkToml`] from a network.toml file. Validates that sharding ports are set
///  if necessary. The number of peers needs to be a multiple of 3.
fn parse_sharded_network_toml(input: &str) -> Result<ShardedNetworkToml, Error> {
    use config::{Config, File, FileFormat};

    let parsed: ShardedNetworkToml = Config::builder()
        .add_source(File::from_str(input, FileFormat::Toml))
        .build()?
        .try_deserialize()?;

    if parsed.peers.len() % 3 != 0 {
        return Err(Error::InvalidNetworkSize(parsed.peers.len()));
    }

    // Validate sharding config is set
    let any_shard_port_set = parsed.peers.iter().any(|peer| peer.shard_port.is_some());
    if any_shard_port_set || parsed.peers.len() > 3 {
        let missing_ports = parsed.missing_shard_ports();
        if !missing_ports.is_empty() {
            return Err(Error::MissingShardPorts(missing_ports));
        }
    }

    Ok(parsed)
}

/// Reads a the config for a specific, single, sharded server from string. Expects config to be
/// toml format. The server in the network is specified via `id`, `shard_index` and
/// `shard_count`. This function expects shard ports to be set for all peers.
///
/// The first 3 peers corresponds to the leaders Ring. H1 shard 0, H2 shard 0, and H3 shard 0.
/// The next 3 correspond to the next ring with `shard_index` equals 1 and so on.
///
/// Other methods to read the network.toml exist depending on the use, for example
/// [`NetworkConfig::from_toml_str`] reads a non-sharded config.
/// TODO: There will be one to read the information relevant for the RC (doesn't need shard
/// info)
///
/// # Errors
/// if `input` is in an invalid format
pub fn sharded_server_from_toml_str(
    input: &str,
    id: HelperIdentity,
    shard_index: ShardIndex,
    shard_count: ShardIndex,
) -> Result<(NetworkConfig<Helper>, NetworkConfig<Shard>), Error> {
    let all_network = parse_sharded_network_toml(input)?;
    let missing_ports = all_network.missing_shard_ports();
    if !missing_ports.is_empty() {
        return Err(Error::MissingShardPorts(missing_ports));
    }

    let ix: usize = shard_index.as_index();
    let ix_count: usize = shard_count.as_index();
    let mpc_id: usize = id.as_index();

    let mpc_network = NetworkConfig {
        peers: all_network
            .peers
            .iter()
            .map(ShardedPeerConfigToml::to_mpc_peer)
            .skip(ix * 3)
            .take(3)
            .collect(),
        client: all_network.client.clone(),
        identities: HelperIdentity::make_three().to_vec(),
    };

    let shard_network = NetworkConfig {
        peers: all_network
            .peers
            .iter()
            .map(ShardedPeerConfigToml::to_shard_peer)
            .skip(mpc_id)
            .step_by(3)
            .take(ix_count)
            .collect(),
        client: all_network.client,
        identities: shard_count.iter().collect(),
    };

    Ok((mpc_network, shard_network))
}

#[derive(Clone, Debug, Deserialize)]
pub struct PeerConfig {
    /// Peer URL
    #[serde(with = "crate::serde::uri")]
    pub url: Uri,

    /// Peer's TLS certificate
    ///
    /// The peer's end-entity TLS certificate must be specified here, unless HTTPS is disabled.
    /// In `network.toml`, the certificate must be in PEM format. It is converted to DER
    /// when the config is loaded.
    ///
    /// Verifying a peer's TLS certificate against the system truststore or a custom root of
    /// trust is not currently supported.
    #[serde(default, deserialize_with = "certificate_from_pem")]
    pub certificate: Option<OwnedCertificate>,

    /// Match key encryption configuration.
    #[serde(default, rename = "hpke")]
    pub hpke_config: Option<HpkeClientConfig>,
}

impl PeerConfig {
    pub fn new(url: Uri, certificate: Option<OwnedCertificate>) -> Self {
        Self {
            url,
            certificate,
            hpke_config: None,
        }
    }
}

/// Match key encryption client configuration. To encrypt match keys towards a helper node, clients
/// need to know helper's public key.
#[derive(Clone, Deserialize)]
pub struct HpkeClientConfig {
    #[serde(deserialize_with = "pk_from_str")]
    pub public_key: IpaPublicKey,
}

impl Debug for HpkeClientConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HpkeClientConfig")
            .field("public_key", &pk_to_str(&self.public_key))
            .finish()
    }
}

impl HpkeClientConfig {
    #[must_use]
    pub fn new(public_key: IpaPublicKey) -> Self {
        Self { public_key }
    }
}

/// Reads a Certificate in PEM format using Serde Serialization
fn certificate_from_pem<'de, D>(deserializer: D) -> Result<Option<OwnedCertificate>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(s) = <Option<String> as Deserialize>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match rustls_pemfile::read_one(&mut s.as_bytes()).map_err(serde::de::Error::custom)? {
        Some(Item::X509Certificate(cert)) => Ok(Some(cert)),
        _ => Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(s.as_ref()),
            &"a certificate",
        )),
    }
}

fn pk_from_str<'de, D>(deserializer: D) -> Result<IpaPublicKey, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    let mut buf = vec![0_u8; 32];
    hex::decode_to_slice(s, &mut buf).map_err(<D::Error as serde::de::Error>::custom)?;

    IpaPublicKey::from_bytes(&buf).map_err(<D::Error as serde::de::Error>::custom)
}

fn pk_to_str(pk: &IpaPublicKey) -> String {
    hex::encode(pk.to_bytes().as_slice())
}

#[derive(Clone, Debug)]
pub enum TlsConfig {
    File {
        /// Path to file containing certificate in PEM format
        certificate_file: PathBuf,

        /// Path to file containing private key in PEM format
        private_key_file: PathBuf,
    },
    Inline {
        /// Certificate in PEM format
        certificate: String,

        // Private key in PEM format
        private_key: String,
    },
}

#[derive(Clone, Debug)]
pub enum HpkeServerConfig {
    File {
        /// Path to file containing private key which decrypts match keys
        private_key_file: PathBuf,
    },
    Inline {
        // Private key in hex format
        private_key: String,
    },
}

/// # Errors
/// If there is a problem with the HPKE configuration.
pub async fn hpke_registry(
    config: Option<&HpkeServerConfig>,
) -> Result<KeyRegistry<PrivateKeyOnly>, BoxError> {
    let sk_str = match config {
        None => return Ok(KeyRegistry::<PrivateKeyOnly>::empty()),
        Some(HpkeServerConfig::Inline { private_key }) => {
            Cow::Borrowed(private_key.trim().as_bytes())
        }
        Some(HpkeServerConfig::File { private_key_file }) => {
            Cow::Owned(fs::read_to_string(private_key_file).await?.trim().into())
        }
    };

    let sk = hex::decode(sk_str)?;

    Ok(KeyRegistry::from_keys([PrivateKeyOnly(
        IpaPrivateKey::from_bytes(&sk)?,
    )]))
}

/// Configuration information for launching an instance of the helper party web service.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Port to listen. If not specified, will ask Kernel to assign the port
    pub port: Option<u16>,

    /// If true, use insecure HTTP. Otherwise (default), use HTTPS.
    pub disable_https: bool,

    /// TLS configuration for helper-to-helper communication
    pub tls: Option<TlsConfig>,

    /// Configuration needed for decrypting match keys
    pub hpke_config: Option<HpkeServerConfig>,
}

pub trait HyperClientConfigurator {
    fn configure<'a>(&self, client_builder: &'a mut Builder) -> &'a mut Builder;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub http_config: HttpClientConfigurator,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig::use_http2()
    }
}

impl ClientConfig {
    #[must_use]
    pub fn use_http2() -> Self {
        Self::configure_http2(Http2Configurator::default())
    }

    #[must_use]
    pub fn configure_http2(conf: Http2Configurator) -> Self {
        Self {
            http_config: HttpClientConfigurator::Http2(conf),
        }
    }

    #[must_use]
    pub fn use_http1() -> Self {
        Self {
            http_config: HttpClientConfigurator::http1(),
        }
    }
}

impl<B: Borrow<ClientConfig>> HyperClientConfigurator for B {
    fn configure<'a>(&self, client_builder: &'a mut Builder) -> &'a mut Builder {
        self.borrow().http_config.configure(client_builder)
    }
}

/// Configure Hyper client to use the specific version of HTTP protocol when communicating with
/// MPC helpers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version")]
#[serde(rename_all = "lowercase")]
pub enum HttpClientConfigurator {
    Http1(Http1Configurator),
    Http2(Http2Configurator),
}

impl HyperClientConfigurator for HttpClientConfigurator {
    fn configure<'a>(&self, client_builder: &'a mut Builder) -> &'a mut Builder {
        match self {
            HttpClientConfigurator::Http1(configurator) => configurator.configure(client_builder),
            HttpClientConfigurator::Http2(configurator) => configurator.configure(client_builder),
        }
    }
}

impl HttpClientConfigurator {
    #[must_use]
    pub fn http1() -> Self {
        Self::Http1(Http1Configurator)
    }

    #[must_use]
    pub fn http2() -> Self {
        Self::Http2(Http2Configurator::default())
    }
}

/// Clients will initiate connections using HTTP/1.1 but can upgrade to use HTTP/2 if server
/// suggests it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Http1Configurator;

impl HyperClientConfigurator for Http1Configurator {
    fn configure<'a>(&self, client_builder: &'a mut Builder) -> &'a mut Builder {
        // See https://github.com/private-attribution/ipa/issues/650
        // and https://github.com/hyperium/hyper/issues/2312
        // This makes it very inefficient to use, so better to avoid HTTP 1.1
        client_builder.pool_max_idle_per_host(0)
    }
}

impl Debug for Http1Configurator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "http version: HTTP 1.1")
    }
}

/// Clients will use HTTP/2 exclusively. This will make client requests fail if server does not
/// support HTTP/2.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Http2Configurator {
    /// Enable [`PING`] frames to keep connection alive. Default value is 90 seconds to match [`Hyper`] value
    /// for `SO_KEEPALIVE`. Note that because
    /// IPA builds [`http`] connector manually, keep-alive is not enabled by Hyper. It is somewhat
    /// confusing that Hyper turns it on inside [`build_http`] method only.
    ///
    /// Enabling PING requires hyper `runtime` feature, so make sure it is enabled. There may be
    /// a bug in hyper that enables method `http2_keep_alive_interval` even when this feature is
    /// turned off. At least I was able to compile IPA without `runtime` feature.
    ///
    /// ## Serialization notes
    ///
    /// IPA uses TOML for configuration files that does not support "unsetting a key": [`toml_issue`].
    /// For this reason, if value is not present in the configuration file, it will be set to `None`.
    /// It is up to the config creator to ensure that value is specified when `network.toml` is created.
    ///
    /// [`PING`]: https://datatracker.ietf.org/doc/html/rfc9113#name-ping
    /// [`Hyper`]: https://docs.rs/hyper/0.14.27/hyper/client/struct.Builder.html#method.pool_idle_timeout
    /// [`http`]: https://docs.rs/hyper/0.14.27/hyper/client/struct.Builder.html#method.build
    /// [`build_http`]: https://docs.rs/hyper/0.14.27/hyper/client/struct.Builder.html#method.build_http
    /// [`toml_issue`]: https://github.com/toml-lang/toml/issues/30
    #[serde(
        rename = "ping_interval_secs",
        default,
        serialize_with = "crate::serde::duration::to_secs",
        deserialize_with = "crate::serde::duration::from_secs_optional",
        skip_serializing_if = "Option::is_none"
    )]
    ping_interval: Option<Duration>,
}

impl Default for Http2Configurator {
    fn default() -> Self {
        Self {
            ping_interval: Some(Duration::from_secs(90)),
        }
    }
}

impl HyperClientConfigurator for Http2Configurator {
    fn configure<'a>(&self, client_builder: &'a mut Builder) -> &'a mut Builder {
        client_builder
            .http2_only(true)
            .http2_keep_alive_interval(self.ping_interval)
    }
}

impl Debug for Http2Configurator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http2Configurator")
            .field("PING_interval", &self.ping_interval)
            .finish()
    }
}

#[derive(Default)]
pub struct KeyRegistries(Vec<KeyRegistry<PublicKeyOnly>>);

impl KeyRegistries {
    /// # Panics
    /// If network file is improperly formatted
    pub fn init_from(
        &mut self,
        network: &NetworkConfig<Helper>,
    ) -> Option<[&KeyRegistry<PublicKeyOnly>; 3]> {
        // Get the configs, if all three peers have one
        let peers = network.peers();
        let configs = peers.iter().try_fold(Vec::new(), |acc, peer| {
            if let (mut vec, Some(hpke_config)) = (acc, peer.hpke_config.as_ref()) {
                vec.push(hpke_config);
                Some(vec)
            } else {
                None
            }
        })?;

        // Create key registries
        self.0 = configs
            .into_iter()
            .map(|hpke| KeyRegistry::from_keys([PublicKeyOnly(hpke.public_key.clone())]))
            .collect::<Vec<KeyRegistry<PublicKeyOnly>>>();

        Some(self.0.iter().collect::<Vec<_>>().try_into().ok().unwrap())
    }
}

#[cfg(all(test, unit_test))]
mod tests {
    use std::time::Duration;

    use hpke::{kem::X25519HkdfSha256, Kem};
    use hyper::Uri;
    use once_cell::sync::Lazy;
    use rand::rngs::StdRng;
    use rand_core::SeedableRng;

    use super::{
        parse_sharded_network_toml, sharded_server_from_toml_str, NetworkConfig, PeerConfig,
    };
    use crate::{
        config::{
            ClientConfig, Error, HpkeClientConfig, Http2Configurator, HttpClientConfigurator,
        },
        helpers::HelperIdentity,
        net::test::TestConfigBuilder,
        sharding::ShardIndex,
        utils::replace_all,
    };

    const URI_1: &str = "http://localhost:3000";
    const URI_1S: &str = "http://localhost:6000";
    const URI_2: &str = "http://localhost:3001";
    const URI_2S: &str = "http://localhost:6001";
    const URI_3: &str = "http://localhost:3002";
    const URI_3S: &str = "http://localhost:6002";

    #[test]
    fn parse_config() {
        let conf = TestConfigBuilder::with_http_and_default_test_ports().build();

        let uri1 = URI_1.parse::<Uri>().unwrap();
        let id1 = HelperIdentity::try_from(1usize).unwrap();
        let ring_value1 = &conf.leaders_ring().network.peers()[id1];
        assert_eq!(ring_value1.url, uri1);
        let uri1s = URI_1S.parse::<Uri>().unwrap();
        let sharding_value1 = conf.get_shards_for_helper(id1).network.get_peer(0).unwrap();
        assert_eq!(sharding_value1.url, uri1s);

        let uri2 = URI_2.parse::<Uri>().unwrap();
        let id2 = HelperIdentity::try_from(2usize).unwrap();
        let ring_value2 = &conf.leaders_ring().network.peers()[id2];
        assert_eq!(ring_value2.url, uri2);
        let uri2s = URI_2S.parse::<Uri>().unwrap();
        let sharding_value2 = conf.get_shards_for_helper(id2).network.get_peer(0).unwrap();
        assert_eq!(sharding_value2.url, uri2s);

        let uri3 = URI_3.parse::<Uri>().unwrap();
        let id3 = HelperIdentity::try_from(3usize).unwrap();
        let ring_value3 = &conf.leaders_ring().network.peers()[id3];
        assert_eq!(ring_value3.url, uri3);
        let uri3s = URI_3S.parse::<Uri>().unwrap();
        let sharding_value3 = conf.get_shards_for_helper(id3).network.get_peer(0).unwrap();
        assert_eq!(sharding_value3.url, uri3s);
    }

    #[test]
    fn debug_hpke_client_config() {
        let mut rng = StdRng::seed_from_u64(1);
        let (_, public_key) = X25519HkdfSha256::gen_keypair(&mut rng);
        let config = HpkeClientConfig { public_key };
        assert_eq!(format!("{config:?}"), "HpkeClientConfig { public_key: \"2bd9da78f01d8bc6948bbcbe44ec1e7163d05083e267d110cdb2e75d847e3b6f\" }");
    }

    #[test]
    fn client_config_serde() {
        fn assert_config_eq(config_str: &str, expected: &ClientConfig) {
            let actual: ClientConfig = serde_json::from_str(config_str).unwrap();

            match (&expected.http_config, &actual.http_config) {
                (HttpClientConfigurator::Http2(left), HttpClientConfigurator::Http2(right)) => {
                    assert_eq!(left, right);
                }
                (HttpClientConfigurator::Http1(_), HttpClientConfigurator::Http1(_)) => {}
                _ => panic!(
                    "http config is not the same: {:?} vs {:?}",
                    expected.http_config, actual.http_config
                ),
            };
        }

        assert!(serde_json::from_str::<ClientConfig>(
            r#"{ "http_config": { "version": "http1", "ping_interval_secs": 132 } }"#,
        )
        .unwrap_err()
        .is_data());

        assert_config_eq(
            r#"{ "http_config": { "version": "http2" } }"#,
            &ClientConfig::configure_http2(Http2Configurator {
                ping_interval: None,
            }),
        );
        assert_config_eq(
            r#"{ "http_config": { "version": "http1" } }"#,
            &ClientConfig::use_http1(),
        );
        assert_config_eq(
            r#"{ "http_config": { "version": "http2", "ping_interval_secs": 132 } }"#,
            &ClientConfig::configure_http2(Http2Configurator {
                ping_interval: Some(Duration::from_secs(132)),
            }),
        );
    }

    #[test]
    fn indexing_peer_happy_case() {
        let uri1 = URI_1.parse::<Uri>().unwrap();
        let pc1 = PeerConfig::new(uri1, None);
        let client = ClientConfig::default();
        let conf = NetworkConfig::new_shards(vec![pc1.clone()], client);
        assert_eq!(conf.peers[ShardIndex(0)].url, pc1.url);
    }

    #[test]
    fn parse_sharded_server_happy() {
        // Asuming position of the second helper in the second shard (the middle server in the 3 x 3)
        let (mpc, shard) = sharded_server_from_toml_str(
            &SHARDED_OK_REPEAT,
            HelperIdentity::TWO,
            ShardIndex::from(1),
            ShardIndex::from(3),
        )
        .unwrap();
        assert_eq!(
            vec![
                "helper1.prod.ipa-helper.shard1.dev:443",
                "helper2.prod.ipa-helper.shard1.dev:443",
                "helper3.prod.ipa-helper.shard1.dev:443"
            ],
            mpc.peers
                .into_iter()
                .map(|p| p.url.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            vec![
                "helper2.prod.ipa-helper.shard0.dev:555",
                "helper2.prod.ipa-helper.shard1.dev:555",
                "helper2.prod.ipa-helper.shard2.dev:555"
            ],
            shard
                .peers
                .into_iter()
                .map(|p| p.url.to_string())
                .collect::<Vec<_>>()
        );
    }

    /// Tests that the url of a shard gets updated with the shard port.
    #[test]
    fn transform_sharded_peers() {
        let mut n = parse_sharded_network_toml(&SHARDED_OK_REPEAT).unwrap();
        assert_eq!(
            "helper3.prod.ipa-helper.shard2.dev:666",
            n.peers.pop().unwrap().to_shard_peer().url
        );
        assert_eq!(
            "helper2.prod.ipa-helper.shard2.dev:555",
            n.peers.pop().unwrap().to_shard_peer().url
        );
    }

    /// Expects an error if the number of peers isn't a multiple of 3
    #[test]
    fn invalid_nr_of_peers() {
        assert!(matches!(
            parse_sharded_network_toml(&SHARDED_8),
            Err(Error::InvalidNetworkSize(_))
        ));
    }

    /// If any sharded port is set (indicating this is a sharding config), then ALL ports must be set.
    #[test]
    fn parse_network_toml_shard_port_some_set() {
        assert!(matches!(
            parse_sharded_network_toml(&SHARDED_COMPAT_ONE_PORT),
            Err(Error::MissingShardPorts(_))
        ));
    }

    /// If there are more than 3 peers configured (indicating this is a sharding config), then ALL ports must be set.
    #[test]
    fn parse_network_toml_shard_port_set() {
        assert!(matches!(
            parse_sharded_network_toml(&SHARDED_MISSING_PORTS_REPEAT),
            Err(Error::MissingShardPorts(_))
        ));
    }

    /// Check that shard ports are given for [`sharded_server_from_toml_str`] or error is returned.
    #[test]
    fn parse_sharded_without_shard_ports() {
        // Second, I test the networkconfig parsing
        assert!(matches!(
            sharded_server_from_toml_str(
                &NON_SHARDED_COMPAT,
                HelperIdentity::TWO,
                ShardIndex::FIRST,
                ShardIndex::from(1)
            ),
            Err(Error::MissingShardPorts(_))
        ));
    }

    /// Testing happy case of a sharded network config
    #[test]
    fn happy_parse_sharded_network_toml() {
        let r_entire_network = parse_sharded_network_toml(SHARDED_OK);
        assert!(r_entire_network.is_ok());
        let entire_network = r_entire_network.unwrap();
        assert!(matches!(
            entire_network.client.http_config,
            HttpClientConfigurator::Http2(_)
        ));
        assert_eq!(3, entire_network.peers.len());
        assert_eq!(
            "helper3.prod.ipa-helper.shard0.dev:443",
            entire_network.peers[2].config.url
        );
        assert_eq!(Some(666), entire_network.peers[2].shard_port);
    }

    /// Testing happy case of a longer sharded network config
    #[test]
    fn happy_parse_larger_sharded_network_toml() {
        let r_entire_network = parse_sharded_network_toml(&SHARDED_OK_REPEAT);
        assert!(r_entire_network.is_ok());
        let entire_network = r_entire_network.unwrap();
        assert_eq!(9, entire_network.peers.len());
        assert_eq!(Some(666), entire_network.peers[8].shard_port);
    }

    /// This test validates that the new logic that handles sharded configurations can also handle the previous version
    #[test]
    fn parse_non_sharded_network_toml() {
        let r_entire_network = parse_sharded_network_toml(&NON_SHARDED_COMPAT);
        assert!(r_entire_network.is_ok());
        let entire_network = r_entire_network.unwrap();
        assert!(matches!(
            entire_network.client.http_config,
            HttpClientConfigurator::Http2(_)
        ));
        assert_eq!(3, entire_network.peers.len());
        assert_eq!(
            "helper3.prod.ipa-helper.dev:443",
            entire_network.peers[2].config.url
        );
    }

    // Following are some large &str const used for tests

    /// Valid: A non-sharded network toml, just how they used to be
    static NON_SHARDED_COMPAT: Lazy<String> = Lazy::new(|| format!("{CLIENT}{P1}{REST}"));

    /// Invalid: Same as [`NON_SHARDED_COMPAT`] but with a single `shard_port` set.
    static SHARDED_COMPAT_ONE_PORT: Lazy<String> =
        Lazy::new(|| format!("{CLIENT}{P1}\nshard_port = 777\n{REST}"));

    /// Helper const used to create client configs
    const CLIENT: &str = "[client.http_config]
ping_interval_secs = 90.0
version = \"http2\"
";

    /// Helper const that has the first part of a Peer, just before were `shard_port` should be
    /// specified.
    const P1: &str = "
[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIIMlnveFys5QUwCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMS5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMwM1oXDTI0
MTIwNDAzMzMwM1owJjEkMCIGA1UEAwwbaGVscGVyMS5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEWmrrkaKM7HQ0Y3ZGJtHB7vfG
cT/hDCXCoob4pJ/fpPDMrqhiwTTck3bNOuzv9QIx+p5C2Qp8u67rYfK78w86NaNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMS5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAKVdDCQeXLRXDYXy4b1N1UxD/JPuD9H7zeRb8/nmIDTfAiBL
a6L0t1Ug8i2RcequSo21x319Tvs5nUbGwzMFSS5wKA==
-----END CERTIFICATE-----
\"\"\"
url = \"helper1.prod.ipa-helper.dev:443\"";

    /// The rest of a configuration
    const REST: &str = "
[peers.hpke]
public_key = \"f458d5e1989b2b8f5dacd4143276aa81eaacf7449744ab1251ff667c43550756\"

[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIITOtoca16QckwCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMi5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMwOFoXDTI0
MTIwNDAzMzMwOFowJjEkMCIGA1UEAwwbaGVscGVyMi5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAETxOH4ATz6kBxLuRznKDFRugm
XKmH7mzRB9wn5vaVlVpDzf4nDHJ+TTzSS6Lb3YLsA7jrXDx+W7xPLGow1+9FNqNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMi5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAI4G5ICVm+v5KK5Y8WVetThtNCXGykUBAM1eE973FBOUAiAS
XXgJe9q9hAfHf0puZbv0j0tGY3BiqCkJJaLvK7ba+g==
-----END CERTIFICATE-----
\"\"\"
url = \"helper2.prod.ipa-helper.dev:443\"

[peers.hpke]
public_key = \"62357179868e5594372b801ddf282c8523806a868a2bff2685f66aa05ffd6c22\"

[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIIaf7eDCnXh2swCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMy5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMxMloXDTI0
MTIwNDAzMzMxMlowJjEkMCIGA1UEAwwbaGVscGVyMy5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEIMqxCCtu4joFr8YtOrEtq230
NuTtUAaJHIHNtv4CvpUcbtlFMWFYUUum7d22A8YTfUeccG5PsjjCoQG/dhhSbKNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMy5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAOTSQWbN7kfIatNJEwWTBL4xOY88E3+SOnBNExCsTkQuAiBB
/cwOQQUEeE4llrDp+EnyGbzmVm5bINz8gePIxkKqog==
-----END CERTIFICATE-----
\"\"\"
url = \"helper3.prod.ipa-helper.dev:443\"

[peers.hpke]
public_key = \"55f87a8794b4de9a60f8ede9ed000f5f10c028e22390922efc4fb63bc6be0a61\"
";

    /// Valid: A sharded configuration
    const SHARDED_OK: &str = "
[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIIMlnveFys5QUwCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMS5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMwM1oXDTI0
MTIwNDAzMzMwM1owJjEkMCIGA1UEAwwbaGVscGVyMS5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEWmrrkaKM7HQ0Y3ZGJtHB7vfG
cT/hDCXCoob4pJ/fpPDMrqhiwTTck3bNOuzv9QIx+p5C2Qp8u67rYfK78w86NaNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMS5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAKVdDCQeXLRXDYXy4b1N1UxD/JPuD9H7zeRb8/nmIDTfAiBL
a6L0t1Ug8i2RcequSo21x319Tvs5nUbGwzMFSS5wKA==
-----END CERTIFICATE-----
\"\"\"
url = \"helper1.prod.ipa-helper.shard0.dev:443\"
shard_port = 444

[peers.hpke]
public_key = \"f458d5e1989b2b8f5dacd4143276aa81eaacf7449744ab1251ff667c43550756\"

[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIITOtoca16QckwCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMi5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMwOFoXDTI0
MTIwNDAzMzMwOFowJjEkMCIGA1UEAwwbaGVscGVyMi5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAETxOH4ATz6kBxLuRznKDFRugm
XKmH7mzRB9wn5vaVlVpDzf4nDHJ+TTzSS6Lb3YLsA7jrXDx+W7xPLGow1+9FNqNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMi5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAI4G5ICVm+v5KK5Y8WVetThtNCXGykUBAM1eE973FBOUAiAS
XXgJe9q9hAfHf0puZbv0j0tGY3BiqCkJJaLvK7ba+g==
-----END CERTIFICATE-----
\"\"\"
url = \"helper2.prod.ipa-helper.shard0.dev:443\"
shard_port = 555

[peers.hpke]
public_key = \"62357179868e5594372b801ddf282c8523806a868a2bff2685f66aa05ffd6c22\"

[[peers]]
certificate = \"\"\"
-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIIaf7eDCnXh2swCgYIKoZIzj0EAwIwJjEkMCIGA1UEAwwb
aGVscGVyMy5wcm9kLmlwYS1oZWxwZXIuZGV2MB4XDTI0MDkwNDAzMzMxMloXDTI0
MTIwNDAzMzMxMlowJjEkMCIGA1UEAwwbaGVscGVyMy5wcm9kLmlwYS1oZWxwZXIu
ZGV2MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEIMqxCCtu4joFr8YtOrEtq230
NuTtUAaJHIHNtv4CvpUcbtlFMWFYUUum7d22A8YTfUeccG5PsjjCoQG/dhhSbKNZ
MFcwJgYDVR0RBB8wHYIbaGVscGVyMy5wcm9kLmlwYS1oZWxwZXIuZGV2MA4GA1Ud
DwEB/wQEAwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZI
zj0EAwIDSAAwRQIhAOTSQWbN7kfIatNJEwWTBL4xOY88E3+SOnBNExCsTkQuAiBB
/cwOQQUEeE4llrDp+EnyGbzmVm5bINz8gePIxkKqog==
-----END CERTIFICATE-----
\"\"\"
url = \"helper3.prod.ipa-helper.shard0.dev:443\"
shard_port = 666

[peers.hpke]
public_key = \"55f87a8794b4de9a60f8ede9ed000f5f10c028e22390922efc4fb63bc6be0a61\"
";

    /// Valid: Three sharded configs together for 9
    static SHARDED_OK_REPEAT: Lazy<String> = Lazy::new(|| {
        format!(
            "{}{}{}",
            SHARDED_OK,
            replace_all(SHARDED_OK, "shard0", "shard1"),
            replace_all(SHARDED_OK, "shard0", "shard2")
        )
    });

    /// Invalid: A network toml with 8 entries
    static SHARDED_8: Lazy<String> = Lazy::new(|| {
        let last_peers_index = SHARDED_OK_REPEAT.rfind("[[peers]]").unwrap();
        SHARDED_OK_REPEAT[..last_peers_index].to_string()
    });

    /// Invalid: Same as [`SHARDED_OK_REPEAT`] but without the expected ports
    static SHARDED_MISSING_PORTS_REPEAT: Lazy<String> = Lazy::new(|| {
        let lines: Vec<&str> = SHARDED_OK_REPEAT.lines().collect();
        let new_lines: Vec<String> = lines
            .iter()
            .filter(|line| !line.starts_with("shard_port ="))
            .map(std::string::ToString::to_string)
            .collect();
        new_lines.join("\n")
    });
}
