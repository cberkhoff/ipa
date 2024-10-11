//! Utilities to generate configurations for unit tests.
//!
//! The convention for unit tests is that H1 is the server, H2 is the client, and H3 is not used
//! other than to write `NetworkConfig`. It is possible that this convention is not universally
//! respected.
//!
//! There is also some test setup for the case of three intercommunicating HTTP helpers in
//! `net::transport::tests`.

#![allow(clippy::missing_panics_doc)]
use std::{
    array,
    iter::zip,
    net::{SocketAddr, TcpListener},
};

use once_cell::sync::Lazy;
use rustls_pki_types::CertificateDer;
use tokio::task::JoinHandle;

use super::client::ClientIdentities;
use crate::{
    config::{
        ClientConfig, HpkeClientConfig, HpkeServerConfig, NetworkConfig, PeerConfig, ServerConfig,
        TlsConfig,
    },
    helpers::{repeat_n, HandlerBox, HelperIdentity, RequestHandler},
    hpke::{Deserializable as _, IpaPublicKey},
    net::{ClientIdentity, MpcHelperClient, MpcHelperServer, MpcHttpTransport},
    sharding::{HelpersRing, IntraHelper, ShardIndex, TransportRestriction},
    sync::Arc,
    test_fixture::metrics::MetricsHandle,
};

pub struct Ports<C> {
    ring: C,
    sharding: C,
}

/// A single ring with 3 hosts, each with a ring and sharding port
pub const DEFAULT_TEST_PORTS: Ports<[u16; 3]> = Ports {
    ring: [3000, 3001, 3002],
    sharding: [6000, 6001, 6002],
};

pub struct AddressableServer {
    /// Cntain the ports
    pub config: ServerConfig,
    /// Sockets are created if no port was specified.
    pub socket: Option<TcpListener>,
}

pub struct Servers {
    pub configs: Vec<AddressableServer>,
}

impl Servers {
    fn new(configs: Vec<ServerConfig>, sockets: Vec<Option<TcpListener>>) -> Self {
        assert_eq!(configs.len(), sockets.len());
        let mut new_configs = Vec::with_capacity(configs.len());
        for s in zip(configs, sockets) {
            new_configs.push(AddressableServer {
                config: s.0,
                socket: s.1,
            });
        }
        Servers {
            configs: new_configs,
        }
    }

    fn push_shard(&mut self, config: AddressableServer) {
        self.configs.push(config);
    }

    pub fn iter(self: &Self) -> impl Iterator<Item = &AddressableServer> {
        self.configs.iter()
    }
}

impl Default for Servers {
    fn default() -> Self {
        Self { configs: vec![] }
    }
}

pub struct RestrictedNetwork<R: TransportRestriction> {
    pub network: NetworkConfig<R>, // Contains Clients
    pub servers: Servers,
}

impl RestrictedNetwork<IntraHelper> {
    pub fn get_first_shard(&self) -> &AddressableServer {
        self.servers.configs.get(0).unwrap()
    }

    pub fn get_first_shard_mut(&mut self) -> &mut AddressableServer {
        self.servers.configs.get_mut(0).unwrap()
    }
}

// If the underlying AddressableServer have socets, then you will want to make this mutable to take the sockets out
pub struct ShardedConfig {
    pub disable_https: bool,
    pub rings: Vec<RestrictedNetwork<HelpersRing>>,
    pub sharding_networks: Vec<RestrictedNetwork<IntraHelper>>,
}

impl ShardedConfig {
    pub fn leaders_ring(&self) -> &RestrictedNetwork<HelpersRing> {
        &self.rings[0]
    }

    pub fn get_shards_for_helper(&self, id: HelperIdentity) -> &RestrictedNetwork<IntraHelper> {
        let ix: usize = usize::try_from(id).unwrap() - 1;
        self.sharding_networks.get::<usize>(ix).unwrap()
    }

    pub fn get_shards_for_helper_mut(
        &mut self,
        id: HelperIdentity,
    ) -> &mut RestrictedNetwork<IntraHelper> {
        let ix: usize = usize::try_from(id).unwrap() - 1;
        self.sharding_networks.get_mut::<usize>(ix).unwrap()
    }
}

impl ShardedConfig {
    #[must_use]
    pub fn builder() -> TestConfigBuilder {
        TestConfigBuilder::default()
    }
}

impl Default for ShardedConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

// TODO: move these standalone functions into a new funcion `TestConfigBuilder::server_config`.
fn get_dummy_matchkey_encryption_info(matchkey_encryption: bool) -> Option<HpkeServerConfig> {
    if matchkey_encryption {
        Some(HpkeServerConfig::Inline {
            private_key: TEST_HPKE_PRIVATE_KEY.to_owned(),
        })
    } else {
        None
    }
}

#[must_use]
fn server_config_insecure_http(port: u16, matchkey_encryption: bool) -> ServerConfig {
    ServerConfig {
        port: Some(port),
        disable_https: true,
        tls: None,
        hpke_config: get_dummy_matchkey_encryption_info(matchkey_encryption),
    }
}

#[must_use]
pub fn server_config_https(
    cert_index: usize,
    port: u16,
    matchkey_encryption: bool,
) -> ServerConfig {
    let (certificate, private_key) = get_test_certificate_and_key(cert_index);
    ServerConfig {
        port: Some(port),
        disable_https: false,
        tls: Some(TlsConfig::Inline {
            certificate: String::from_utf8(certificate.to_owned()).unwrap(),
            private_key: String::from_utf8(private_key.to_owned()).unwrap(),
        }),
        hpke_config: get_dummy_matchkey_encryption_info(matchkey_encryption),
    }
}

#[derive(Default)]
pub struct TestConfigBuilder {
    /// Can be empty meaning that free ports should be obtained from os
    /// rings > sharding | ring > 3 hosts
    ports_by_ring: Vec<Ports<Vec<u16>>>,
    /// Describes the network
    sharding_factor: usize,
    disable_https: bool,
    use_http1: bool,
    disable_matchkey_encryption: bool,
}

impl TestConfigBuilder {
    #[must_use]
    pub fn with_http_and_default_test_ports() -> Self {
        Self {
            ports_by_ring: vec![Ports {
                ring: DEFAULT_TEST_PORTS.ring.to_vec(),
                sharding: DEFAULT_TEST_PORTS.sharding.to_vec(),
            }],
            sharding_factor: 1,
            disable_https: true,
            use_http1: false,
            disable_matchkey_encryption: false,
        }
    }

    #[must_use]
    pub fn with_open_ports() -> Self {
        Self {
            ports_by_ring: vec![],
            sharding_factor: 1,
            disable_https: false,
            use_http1: false,
            disable_matchkey_encryption: false,
        }
    }

    #[must_use]
    pub fn with_disable_https_option(mut self, value: bool) -> Self {
        self.disable_https = value;
        self
    }

    #[must_use]
    pub fn with_use_http1_option(mut self, value: bool) -> Self {
        self.use_http1 = value;
        self
    }

    #[allow(dead_code)]
    #[must_use]
    // TODO(richaj) Add tests for checking the handling of this. At present the code to decrypt does not exist.
    pub fn disable_matchkey_encryption(mut self) -> Self {
        self.disable_matchkey_encryption = true;
        self
    }

    /// Not using arrays since it makes the passing around harder
    fn build_three(&self, optional_ports: Option<Vec<u16>>, ring_index: usize) -> Servers {
        let mut sockets = vec![None, None, None];
        let ports = optional_ports.unwrap_or_else(|| {
            sockets = (0..3)
                .map(|_| Some(TcpListener::bind("localhost:0").unwrap()))
                .collect();
            sockets
                .iter()
                .map(|sock| sock.as_ref().unwrap().local_addr().unwrap().port())
                .collect()
        });
        // Ports will always be defined after this. Socks only if there were no ports set.
        let configs = if self.disable_https {
            ports
                .into_iter()
                .map(|ports| server_config_insecure_http(ports, !self.disable_matchkey_encryption))
                .collect()
        } else {
            let start_idx = ring_index * 3;
            (start_idx..start_idx + 3)
                .map(|id| server_config_https(id, ports[id], !self.disable_matchkey_encryption))
                .collect()
        };
        Servers::new(configs, sockets)
    }

    fn build_network<R: TransportRestriction>(
        &self,
        servers: Servers,
        scheme: &str,
        certs: Vec<Option<CertificateDer<'static>>>,
    ) -> RestrictedNetwork<R> {
        let peers = certs
            .into_iter()
            .enumerate()
            .map(|(i, cert)| PeerConfig {
                url: format!(
                    "{scheme}://localhost:{}",
                    servers.configs[i]
                        .config
                        .port
                        .expect("Port should have been defined by build_three")
                )
                .parse()
                .unwrap(),
                certificate: cert,
                hpke_config: if self.disable_matchkey_encryption {
                    None
                } else {
                    Some(HpkeClientConfig::new(
                        IpaPublicKey::from_bytes(
                            &hex::decode(TEST_HPKE_PUBLIC_KEY.trim()).unwrap(),
                        )
                        .unwrap(),
                    ))
                },
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let network = NetworkConfig::<R>::new(
            peers,
            self.use_http1
                .then(ClientConfig::use_http1)
                .unwrap_or_default(),
        );
        RestrictedNetwork { network, servers }
    }

    #[must_use]
    pub fn build(self) -> ShardedConfig {
        // first we build all the rings and then we connect all the shards
        let mut sharding_networks_shards =
            vec![Servers::default(), Servers::default(), Servers::default()];
        let mut rings: Vec<RestrictedNetwork<HelpersRing>> = vec![];
        let mut sharding_networks: Vec<RestrictedNetwork<IntraHelper>> = vec![];
        for s in 0..self.sharding_factor {
            let ring_ports = self.ports_by_ring.get(s).map(|p| p.ring.clone());
            let shards_ports = self.ports_by_ring.get(s).map(|p| p.sharding.clone());

            // We create the servers in the MPC ring and connect them.
            let ring_servers = self.build_three(ring_ports, s);
            let (scheme, certs) = if self.disable_https {
                ("http", [None, None, None])
            } else {
                ("https", get_certs_der_row(s))
            };
            let ring_network = self.build_network(ring_servers, scheme, certs.to_vec());
            rings.push(ring_network);

            // We create the sharding servers and accumulate them but don't connect them yet
            let shard_servers = self.build_three(shards_ports, s);
            for (i, s) in shard_servers.configs.into_iter().enumerate() {
                sharding_networks_shards[i].push_shard(s);
            }
        }
        for (i, s) in sharding_networks_shards.into_iter().enumerate() {
            let (scheme, certs) = if self.disable_https {
                ("http", repeat_n(None, self.sharding_factor).collect())
            } else {
                ("https", get_certs_der_col(i).to_vec())
            };
            sharding_networks.push(self.build_network(s, scheme, certs));
        }

        ShardedConfig {
            disable_https: self.disable_https,
            rings,
            sharding_networks,
        }
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub handle: JoinHandle<()>,
    pub transport: MpcHttpTransport,
    pub server: MpcHelperServer<HelpersRing>,
    pub client: MpcHelperClient,
    pub request_handler: Option<Arc<dyn RequestHandler<HelperIdentity>>>,
}
/// Test for ring interactions
impl TestServer {
    /// Build default set of test clients
    ///
    /// All three clients will be configured with the same default server URL, thus,
    /// at most one client will do anything useful.
    pub async fn default() -> TestServer {
        Self::builder().build().await
    }

    /// Return a test client builder
    #[must_use]
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::default()
    }
}

#[derive(Default)]
pub struct TestServerBuilder {
    handler: Option<Arc<dyn RequestHandler<HelperIdentity>>>,
    metrics: Option<MetricsHandle>,
    disable_https: bool,
    use_http1: bool,
    disable_matchkey_encryption: bool,
}

impl TestServerBuilder {
    #[must_use]
    pub fn with_request_handler(
        mut self,
        handler: Arc<dyn RequestHandler<HelperIdentity>>,
    ) -> Self {
        self.handler = Some(handler);
        self
    }

    #[cfg(all(test, unit_test))]
    #[must_use]
    pub fn with_metrics(mut self, metrics: MetricsHandle) -> Self {
        self.metrics = Some(metrics);
        self
    }

    #[must_use]
    pub fn disable_https(mut self) -> Self {
        self.disable_https = true;
        self
    }

    #[allow(dead_code)]
    #[must_use]
    // TODO(richaj) Add tests for checking the handling of this. At present the code to decrypt does not exist.
    pub fn disable_matchkey_encryption(mut self) -> Self {
        self.disable_matchkey_encryption = true;
        self
    }

    #[cfg(all(test, web_test))]
    #[must_use]
    pub fn use_http1(mut self) -> Self {
        self.use_http1 = true;
        self
    }

    pub async fn build(self) -> TestServer {
        let identities = create_ids(self.disable_https, HelperIdentity::ONE, ShardIndex::FIRST);
        let mut test_config = ShardedConfig::builder()
            .with_disable_https_option(self.disable_https)
            .with_use_http1_option(self.use_http1)
            // TODO: add disble_matchkey here
            .build();
        let leaders_ring = test_config.rings.pop().unwrap();
        let first_server = leaders_ring.servers.configs.into_iter().next().unwrap();
        let clients =
            MpcHelperClient::from_conf(&leaders_ring.network, &identities.helper.clone_with_key());
        let handler = self.handler.as_ref().map(HandlerBox::owning_ref);
        let client = clients[0].clone();
        let (transport, server) = MpcHttpTransport::new(
            HelperIdentity::ONE,
            first_server.config,
            leaders_ring.network.clone(),
            clients,
            handler,
        );
        let (addr, handle) = server.start_on(first_server.socket, self.metrics).await;
        // Get the config for HelperIdentity::ONE
        //let h1_peer_config = leaders_ring.network.peers().into_iter().next().unwrap();
        // At some point it might be appropriate to return two clients here -- the first being
        // another helper and the second being a report collector. For now we use the same client
        // for both types of calls.
        //let client = MpcHelperClient::new(&leaders_ring.network.client, h1_peer_config, identity);
        TestServer {
            addr,
            handle,
            transport,
            server,
            client,
            request_handler: self.handler,
        }
    }
}

pub fn create_ids(disable_https: bool, id: HelperIdentity, ix: ShardIndex) -> ClientIdentities {
    if disable_https {
        ClientIdentities::new_headers(id, ix)
    } else {
        get_client_test_identity(id, ix)
    }
}

fn get_test_certificate_and_key(id: usize) -> (&'static [u8], &'static [u8]) {
    (TEST_CERTS[id], TEST_KEYS[id])
}

#[must_use]
pub fn get_client_test_identity(id: HelperIdentity, ix: ShardIndex) -> ClientIdentities {
    let col: usize = usize::try_from(id).unwrap() - 1;
    let row: usize = usize::try_from(ix).unwrap();
    let (mut certificate, mut private_key) = get_test_certificate_and_key(row * 3 + col);
    ClientIdentities {
        helper: ClientIdentity::from_pkcs8(&mut certificate, &mut private_key).unwrap(),
        shard: ClientIdentity::from_pkcs8(&mut certificate, &mut private_key).unwrap(),
    }
}

pub const TEST_CERTS: [&[u8]; 6] = [
    b"\
-----BEGIN CERTIFICATE-----
MIIBZjCCAQ2gAwIBAgIIGGCAUnB4cZcwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTIzMDgxNTE3MDEzM1oYDzIwNzMwODAyMTcwMTMzWjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQulPXT
7xgX8ujzmgRHojfPAx7udp+4rXIwreV2CpvsqHJfjF+tqhPYI9VVJwKXpCEyWMyo
PcCnjX7t22nJt7Zuo0cwRTAUBgNVHREEDTALgglsb2NhbGhvc3QwDgYDVR0PAQH/
BAQDAgKkMB0GA1UdJQQWMBQGCCsGAQUFBwMBBggrBgEFBQcDAjAKBggqhkjOPQQD
AgNHADBEAiAM9p5IUpI0/7vcCNZUebOvXogBKP8XOQ2MzLGq+hD/aQIgU7FXX6BO
MTmpcAH905PiJnhKrEJyGESyfv0D8jGZJXw=
-----END CERTIFICATE-----
",
    b"\
-----BEGIN CERTIFICATE-----
MIIBZjCCAQ2gAwIBAgIILilUFFCeLaowCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTIzMDgxNTE3MDEzM1oYDzIwNzMwODAyMTcwMTMzWjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQkeRc+
xcqwKtwc7KXfiz0qfRX1roD+ESxMP7GWIuJinNoJCKOUw2pVqJTHp86sk6BHTD3E
ULlYJ2fjKR/ogsZPo0cwRTAUBgNVHREEDTALgglsb2NhbGhvc3QwDgYDVR0PAQH/
BAQDAgKkMB0GA1UdJQQWMBQGCCsGAQUFBwMBBggrBgEFBQcDAjAKBggqhkjOPQQD
AgNHADBEAiBuUib76qjK9aDHd7nD5LWE3V4WeBhwDktaDED5qmqHUgIgXCBJn8Fh
fqkn1QdTcGapzuMJqmhMzYUPeRJ4Vr1h7HA=
-----END CERTIFICATE-----
",
    b"\
-----BEGIN CERTIFICATE-----
MIIBZjCCAQ2gAwIBAgIIbYdpxPgluuUwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTIzMDgxNTE3MDEzM1oYDzIwNzMwODAyMTcwMTMzWjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASEORA/
IDvqRGiJpddoyocRa+9HEG2B6P8vfTTV28Ph7n9YBgJodGd29Kt7Dy2IdCjy7PsO
ik5KGZ4Ee+a+juKko0cwRTAUBgNVHREEDTALgglsb2NhbGhvc3QwDgYDVR0PAQH/
BAQDAgKkMB0GA1UdJQQWMBQGCCsGAQUFBwMBBggrBgEFBQcDAjAKBggqhkjOPQQD
AgNHADBEAiB+K2yadiLIDR7ZvDpyMIXP70gL3CXp7JmVmh8ygFtbjQIgU16wnFBy
jn+NXYPeKEWnkCcVKjFED6MevGnOgrJylgY=
-----END CERTIFICATE-----
",
    b"
-----BEGIN CERTIFICATE-----
MIIBZDCCAQugAwIBAgIIFeKzq6ypfYgwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MB4XDTI0MTAwNjIyMTEzOFoXDTI1MDEwNTIyMTEzOFowFDESMBAG
A1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAECKdJUHmm
Mmqtvhu4PpWwwZnu+LFjaE8Y9guDNIXN+O9kulFl1hLVMx6WLpoScrLYlvHrQvcq
/BTG24EOKAeaRqNHMEUwFAYDVR0RBA0wC4IJbG9jYWxob3N0MA4GA1UdDwEB/wQE
AwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZIzj0EAwID
RwAwRAIgBO2SBoLmPikfcovOFpjA8jpY+JuSybeISUKD2GAsXQICIEChXm7/UJ7p
86qXEVsjN2N1pyRd6rUNxLyCaV87ZmfS
-----END CERTIFICATE-----
",
    b"
-----BEGIN CERTIFICATE-----
MIIBZTCCAQugAwIBAgIIXTgB/bkN/aUwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MB4XDTI0MTAwNjIyMTIwM1oXDTI1MDEwNTIyMTIwM1owFDESMBAG
A1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyzSofZIX
XgLUKGumrN3SEXOMOAKXcl1VshTBzvyVwxxnD01WVLgS80/TELEltT8SMj1Cgu7I
tkDx3EVPjq4pOKNHMEUwFAYDVR0RBA0wC4IJbG9jYWxob3N0MA4GA1UdDwEB/wQE
AwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZIzj0EAwID
SAAwRQIhAN93g0zfB/4VyhNOaY1uCb4af4qMxcz1wp0yZ7HKAyWqAiBVPgv4X7aR
JMepVZwIWJrVhnxdcmzOuONoeLZPZraFpw==
-----END CERTIFICATE-----
",
    b"
-----BEGIN CERTIFICATE-----
MIIBZTCCAQugAwIBAgIIXTgB/bkN/aUwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MB4XDTI0MTAwNjIyMTIwM1oXDTI1MDEwNTIyMTIwM1owFDESMBAG
A1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyzSofZIX
XgLUKGumrN3SEXOMOAKXcl1VshTBzvyVwxxnD01WVLgS80/TELEltT8SMj1Cgu7I
tkDx3EVPjq4pOKNHMEUwFAYDVR0RBA0wC4IJbG9jYWxob3N0MA4GA1UdDwEB/wQE
AwICpDAdBgNVHSUEFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwCgYIKoZIzj0EAwID
SAAwRQIhAN93g0zfB/4VyhNOaY1uCb4af4qMxcz1wp0yZ7HKAyWqAiBVPgv4X7aR
JMepVZwIWJrVhnxdcmzOuONoeLZPZraFpw==
-----END CERTIFICATE-----
",
];

pub static TEST_CERTS_DER: Lazy<[CertificateDer; 6]> = Lazy::new(|| {
    TEST_CERTS.map(|mut pem| rustls_pemfile::certs(&mut pem).flatten().next().unwrap())
});

pub fn get_certs_der_row(ring_index: usize) -> [Option<CertificateDer<'static>>; 3] {
    let r: [usize; 3] = array::from_fn(|i| i + ring_index * 3);
    r.map(|i| Some(TEST_CERTS_DER[i].clone()))
}

pub fn get_certs_der_col(helper: usize) -> [Option<CertificateDer<'static>>; 2] {
    let r: [usize; 2] = array::from_fn(|i| i * 3 + helper);
    r.map(|i| Some(TEST_CERTS_DER[i].clone()))
}

pub const TEST_KEYS: [&[u8]; 6] = [
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgHmPeGcv6Dy9QWPHD
ZU7CA+ium1zctVC4HZnrhFlfdiGhRANCAAQulPXT7xgX8ujzmgRHojfPAx7udp+4
rXIwreV2CpvsqHJfjF+tqhPYI9VVJwKXpCEyWMyoPcCnjX7t22nJt7Zu
-----END PRIVATE KEY-----
",
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgvoE0RVtf/0DuE5qt
AimoTcGcGA7dRgq70Ycp0VX2qTqhRANCAAQkeRc+xcqwKtwc7KXfiz0qfRX1roD+
ESxMP7GWIuJinNoJCKOUw2pVqJTHp86sk6BHTD3EULlYJ2fjKR/ogsZP
-----END PRIVATE KEY-----
",
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgDfOsXGbO9T6e9mPb
u9BeVKo7j/DyX4j3XcqrOYnIwOOhRANCAASEORA/IDvqRGiJpddoyocRa+9HEG2B
6P8vfTTV28Ph7n9YBgJodGd29Kt7Dy2IdCjy7PsOik5KGZ4Ee+a+juKk
-----END PRIVATE KEY-----
",
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgWlbBJGC40HwzwMsd
3a6o6x75HZgRnktVwBoi6/84nPmhRANCAAQIp0lQeaYyaq2+G7g+lbDBme74sWNo
Txj2C4M0hc3472S6UWXWEtUzHpYumhJystiW8etC9yr8FMbbgQ4oB5pG
-----END PRIVATE KEY-----
",
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgi9TsF4lX49P+GIER
DjyUhMiyRZ52EsD00dGPRA4XJbahRANCAATLNKh9khdeAtQoa6as3dIRc4w4Apdy
XVWyFMHO/JXDHGcPTVZUuBLzT9MQsSW1PxIyPUKC7si2QPHcRU+Orik4
-----END PRIVATE KEY-----
",
    b"\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgs8cH8I4hrdrqDN/d
p1HENqJEFXMwcERH5JFyW/B6D/ChRANCAAT+nXv66H0vd2omUjYwWDYbGkIiGc6S
jzcyiSIULkaelVYvnEQBYefjGLQwvwbifmMrQ+hfQUT9FNbGRQ788pK9
-----END PRIVATE KEY-----
",
];

// Yes, these strings have trailing newlines. Things that consume them
// should strip whitespace.
const TEST_HPKE_PUBLIC_KEY: &str = "\
0ef21c2f73e6fac215ea8ec24d39d4b77836d09b1cf9aeb2257ddd181d7e663d
";

const TEST_HPKE_PRIVATE_KEY: &str = "\
a0778c3e9960576cbef4312a3b7ca34137880fd588c11047bd8b6a8b70b5a151
";
