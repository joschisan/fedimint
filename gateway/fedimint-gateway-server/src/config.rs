use std::net::SocketAddr;
use std::path::PathBuf;

use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::util::SafeUrl;
use fedimint_gateway_common::{PaymentFee, V1_API_ENDPOINT};

use super::envs;
use crate::envs::{
    FM_BITCOIND_PASSWORD_ENV, FM_BITCOIND_URL_ENV, FM_BITCOIND_USERNAME_ENV, FM_ESPLORA_URL_ENV,
    FM_GATEWAY_METRICS_LISTEN_ADDR_ENV,
};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DatabaseBackend {
    /// Use RocksDB database backend
    #[value(name = "rocksdb")]
    RocksDb,
    /// Use CursedRedb database backend (hybrid memory/redb)
    #[value(name = "cursed-redb")]
    CursedRedb,
}

/// Command line parameters for starting the gateway. `mode`, `data_dir`,
/// `listen`, and `api_addr` are all required.
#[derive(Parser)]
#[command(version)]
#[command(
    group(
        ArgGroup::new("bitcoind_password_auth")
           .args(["bitcoind_password"])
           .multiple(false)
    ),
    group(
        ArgGroup::new("bitcoind_auth")
            .args(["bitcoind_url"])
            .requires("bitcoind_password_auth")
            .requires_all(["bitcoind_username", "bitcoind_url"])
    ),
    group(
        ArgGroup::new("bitcoin_rpc")
            .required(true)
            .multiple(true)
            .args(["bitcoind_url", "esplora_url"])
    )
)]
pub struct GatewayOpts {
    /// Path to folder containing gateway config and data files
    #[arg(long = "data-dir", env = envs::FM_GATEWAY_DATA_DIR_ENV)]
    pub data_dir: PathBuf,

    /// Gateway webserver listen address
    #[arg(long = "listen", env = envs::FM_GATEWAY_LISTEN_ADDR_ENV)]
    listen: SocketAddr,

    /// Public URL from which the webserver API is reachable
    #[arg(long = "api-addr", env = envs::FM_GATEWAY_API_ADDR_ENV)]
    api_addr: Option<SafeUrl>,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = envs::FM_GATEWAY_NETWORK_ENV)]
    network: Network,

    /// Database backend to use.
    #[arg(long, env = envs::FM_DB_BACKEND_ENV, value_enum, default_value = "rocksdb")]
    pub db_backend: DatabaseBackend,

    /// The username to use when connecting to bitcoind
    #[arg(long, env = FM_BITCOIND_USERNAME_ENV)]
    pub bitcoind_username: Option<String>,

    /// The password to use when connecting to bitcoind
    #[arg(long, env = FM_BITCOIND_PASSWORD_ENV)]
    pub bitcoind_password: Option<String>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    /// This should not include authentication parameters, they should be
    /// included in `FM_BITCOIND_USERNAME` and `FM_BITCOIND_PASSWORD`
    #[arg(long, env = FM_BITCOIND_URL_ENV)]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = FM_ESPLORA_URL_ENV)]
    pub esplora_url: Option<SafeUrl>,

    /// LDK lightning node listen port
    #[arg(
        long = "ldk-lightning-port",
        env = "FM_PORT_LDK",
        default_value_t = 9735
    )]
    pub lightning_port: u16,

    /// LDK node alias
    #[arg(long = "ldk-alias", env = "FM_LDK_ALIAS", default_value = "")]
    pub ldk_alias: String,

    /// The default routing fees that are applied to new federations
    #[arg(long = "default-routing-fees", env = envs::FM_DEFAULT_ROUTING_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = envs::FM_DEFAULT_TRANSACTION_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    default_transaction_fees: PaymentFee,

    /// Gateway metrics listen address. If not set, defaults to localhost on the
    /// UI port + 1.
    #[arg(long = "metrics-listen", env = FM_GATEWAY_METRICS_LISTEN_ADDR_ENV)]
    metrics_listen: Option<SocketAddr>,
}

impl GatewayOpts {
    /// Converts the command line parameters into a helper struct the Gateway
    /// uses to store runtime parameters.
    pub fn to_gateway_parameters(&self) -> anyhow::Result<GatewayParameters> {
        let versioned_api = self.api_addr.clone().map(|api_addr| {
            api_addr
                .join(V1_API_ENDPOINT)
                .expect("Could not join v1 api_addr")
        });

        // Default metrics listen to localhost on UI port + 1
        let metrics_listen = self.metrics_listen.unwrap_or_else(|| {
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                self.listen.port() + 1,
            )
        });

        Ok(GatewayParameters {
            listen: self.listen,
            versioned_api,
            network: self.network,
            default_routing_fees: self.default_routing_fees,
            default_transaction_fees: self.default_transaction_fees,
            metrics_listen,
        })
    }
}

/// `GatewayParameters` is a helper struct that can be derived from
/// `GatewayOpts` that holds the CLI or environment variables that are specified
/// by the user.
///
/// If `GatewayConfiguration is set in the database, that takes precedence and
/// the optional parameters will have no affect.
#[derive(Debug)]
pub struct GatewayParameters {
    pub listen: SocketAddr,
    pub versioned_api: Option<SafeUrl>,
    pub network: Network,
    pub default_routing_fees: PaymentFee,
    pub default_transaction_fees: PaymentFee,
    pub metrics_listen: SocketAddr,
}
