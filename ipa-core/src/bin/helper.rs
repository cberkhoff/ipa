use std::{
    collections::HashMap,
    fs,
    io::BufReader,
    net::TcpListener,
    num::NonZeroUsize,
    os::fd::{FromRawFd, RawFd},
    path::{Path, PathBuf},
    process,
};

use clap::{self, Parser, Subcommand};
use hyper::http::uri::Scheme;
use ipa_core::{
    cli::{
        client_config_setup, keygen, test_setup, ConfGenArgs, KeygenArgs, TestSetupArgs, Verbosity,
    },
    config::{hpke_registry, HpkeServerConfig, NetworkConfig, ServerConfig, ShardsConfig, TlsConfig},
    error::BoxError,
    helpers::HelperIdentity,
    net::{ClientIdentity, MpcHelperClient, MpcHttpTransport, ShardHttpTransport},
    sharding::ShardIndex,
    AppConfig, AppSetup,
};
use tracing::{error, info};

#[cfg(all(not(target_env = "msvc"), not(target_os = "macos")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Parser)]
#[clap(
    name = "helper",
    about = "Interoperable Private Attribution (IPA) MPC helper"
)]
#[command(subcommand_negates_reqs = true)]
struct Args {
    /// Configure logging.
    #[clap(flatten)]
    logging: Verbosity,

    #[clap(flatten, next_help_heading = "Server Options")]
    server: ServerArgs,

    #[command(subcommand)]
    command: Option<HelperCommand>,
}

#[derive(Debug, clap::Args)]
struct ServerArgs {
    /// Identity of this helper in the MPC protocol (1, 2, or 3)
    // This is required when running the server, but the `subcommand_negates_reqs`
    // attribute on `Args` makes it optional when running a utility command.
    #[arg(short, long, required = true)]
    identity: Option<usize>,

    /// Index of this shard within a helper.
    #[arg(long, default_value = "0", requires = "total_shards")]
    index: u32,

    /// Index of this shard within a helper.
    #[arg(long, default_value = "1", requires = "index")]
    total_shards: u32,

    /// Port to listen on for helper-to-helper communication.
    #[arg(short, long, visible_alias("port"), default_value = "3000")]
    ring_port: Option<u16>,

    /// Port to listen for shard communication
    #[arg(short, long, default_value = "6000")]
    shard_port: Option<u16>,

    /// Use the supplied prebound socket instead of binding a new socket
    ///
    /// This is only intended for avoiding port conflicts in tests.
    #[arg(hide = true, visible_alias("server_socket_fd"), long)]
    ring_server_socket_fd: Option<RawFd>,

    /// Use the supplied prebound socket instead of binding a new socket for inter shard communication.
    ///
    /// This is only intended for avoiding port conflicts in tests.
    #[arg(hide = true, long)]
    shard_server_socket_fd: Option<RawFd>,

    /// Use insecure HTTP
    #[arg(short = 'k', long)]
    disable_https: bool,

    /// File containing the ring network configuration
    #[arg(long, visible_alias("network"), required = true)]
    ring_network: Option<PathBuf>,

    #[arg(long, required = true)]
    shard_network: Option<PathBuf>,

    /// TLS certificate for helper-to-helper communication
    #[arg(
        long,
        visible_alias("cert"),
        visible_alias("tls-certificate"),
        visible_alias("tls-cert"),
        requires = "tls_key"
    )]
    ring_tls_cert: Option<PathBuf>,

    /// TLS key for helper-to-helper communication
    #[arg(long, visible_alias("key"), visible_alias("tls_key"), requires = "ring_tls_cert")]
    ring_tls_key: Option<PathBuf>,

    /// TLS certificate for intra-helper communication
    #[arg(
        long,
        requires = "shard_tls_key"
    )]
    shard_tls_cert: Option<PathBuf>,

    /// TLS key for intra-helper communication
    #[arg(long, visible_alias("key"), visible_alias("tls_key"), requires = "shard_tls_cert")]
    shard_tls_key: Option<PathBuf>,

    /// Public key for encrypting match keys
    #[arg(long, requires = "mk_private_key")]
    mk_public_key: Option<PathBuf>,

    /// Private key for decrypting match keys
    #[arg(long, requires = "mk_public_key")]
    mk_private_key: Option<PathBuf>,

    /// Override the amount of active work processed in parallel
    #[arg(long)]
    active_work: Option<NonZeroUsize>,
}

#[derive(Debug, Subcommand)]
enum HelperCommand {
    Confgen(ConfGenArgs),
    Keygen(KeygenArgs),
    TestSetup(TestSetupArgs),
}

fn read_file(path: &Path) -> Result<BufReader<fs::File>, BoxError> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map(BufReader::new)
        .map_err(|e| format!("failed to open file {}: {e:?}", path.display()))?)
}

async fn server(args: ServerArgs) -> Result<(), BoxError> {
    let mpc_identity = HelperIdentity::try_from(args.identity.expect("enforced by clap")).unwrap();
    let shard_index = ShardIndex(args.index);

    let (mpc_identity, mpc_server_tls) = match (args.ring_tls_cert, args.ring_tls_key) {
        (Some(ring_cert_file), Some(ring_key_file)) => {
            let mut key = read_file(&ring_key_file)?;
            let mut certs = read_file(&ring_cert_file)?;
            (
                ClientIdentity::from_pkcs8(&mut certs, &mut key)?,
                Some(TlsConfig::File {
                    certificate_file: ring_cert_file,
                    private_key_file: ring_key_file,
                }),
            )
        }
        (None, None) => (ClientIdentity::Helper(mpc_identity), None),
        _ => panic!("should have been rejected by clap"),
    };

    let (shard_identity, shard_server_tls) = match (args.shard_tls_cert, args.shard_tls_key) {
        (Some(shard_cert_file), Some(shard_key_file)) => {
            let mut key = read_file(&shard_key_file)?;
            let mut certs = read_file(&shard_cert_file)?;
            (
                ClientIdentity::from_pkcs8(&mut certs, &mut key)?,
                Some(TlsConfig::File {
                    certificate_file: shard_cert_file,
                    private_key_file: shard_key_file,
                }),
            )
        }
        (None, None) => (ClientIdentity::Shard(shard_index), None),
        _ => panic!("should have been rejected by clap"),
    };

    let mk_encryption = args.mk_private_key.map(|sk_path| HpkeServerConfig::File {
        private_key_file: sk_path,
    });

    let app_config = AppConfig::default()
        .with_key_registry(hpke_registry(mk_encryption.as_ref()).await?)
        .with_active_work(args.active_work);
    let (setup, mpc_handler, shard_handler) = AppSetup::new(app_config);

    let mpc_server_config = ServerConfig {
        port: args.ring_port,
        disable_https: args.disable_https,
        tls: mpc_server_tls,
        hpke_config: mk_encryption,
    };

    let shard_server_config = ServerConfig {
        port: args.shard_port,
        disable_https: args.disable_https,
        tls: shard_server_tls,
        hpke_config: mk_encryption,
    };

    let scheme = if args.disable_https {
        Scheme::HTTP
    } else {
        Scheme::HTTPS
    };
    let network_config_path = args.ring_network.as_deref().unwrap();
    let network_config = NetworkConfig::from_toml_str(&fs::read_to_string(network_config_path)?)?
        .override_scheme(&scheme);
    let clients = MpcHelperClient::from_conf(&network_config, &mpc_identity);

    let (transport, server) = MpcHttpTransport::new(
        mpc_identity,
        mpc_server_config.clone(),
        network_config.clone(),
        clients,
        Some(mpc_handler),
    );

    let shards_config = ShardsConfig {
        peers: vec![],
        client: network_config.client,
    };

    let shard_transport = ShardHttpTransport::new(ShardIndex(0u32), shard_server_config,shards_config, HashMap::new(), None);

    let _app = setup.connect(transport.clone(), shard_transport);

    let listener = args.ring_server_socket_fd
        .map(|fd| {
            // SAFETY:
            //  1. The `--server-socket-fd` option is only intended for use in tests, not in production.
            //  2. This must be the only call to from_raw_fd for this file descriptor, to ensure it has
            //     only one owner.
            let listener = unsafe { TcpListener::from_raw_fd(fd) };
            if listener.local_addr().is_ok() {
                info!("adopting fd {fd} as listening socket");
                Ok(listener)
            } else {
                Err(BoxError::from(format!("the server was asked to listen on fd {fd}, but it does not appear to be a valid socket")))
            }
        })
        .transpose()?;

    let (_addr, server_handle) = server
        .start_on(
            listener,
            // TODO, trace based on the content of the query.
            None as Option<()>,
        )
        .await;

    server_handle.await?;

    Ok(())
}

#[tokio::main]
pub async fn main() {
    let args = Args::parse();
    let _handle = args.logging.setup_logging();

    let res = match args.command {
        None => server(args.server).await,
        Some(HelperCommand::Keygen(args)) => keygen(&args),
        Some(HelperCommand::TestSetup(args)) => test_setup(args),
        Some(HelperCommand::Confgen(args)) => client_config_setup(args),
    };

    if let Err(e) = res {
        error!("{e}");
        process::exit(1);
    }
}
