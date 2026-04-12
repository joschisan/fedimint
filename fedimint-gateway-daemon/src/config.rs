use std::net::SocketAddr;
use std::path::PathBuf;

use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::util::SafeUrl;
use fedimint_gateway_common::{PaymentFee, V1_API_ENDPOINT};

/// Command line parameters for starting the gateway.
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
    #[arg(long = "data-dir", env = "FM_GATEWAY_DATA_DIR")]
    pub data_dir: PathBuf,

    /// Public API listen address
    #[arg(
        long = "api-bind",
        env = "FM_GATEWAY_API_BIND",
        default_value = "0.0.0.0:8175"
    )]
    pub api_bind: SocketAddr,

    /// CLI/admin listen address
    #[arg(
        long = "cli-bind",
        env = "FM_GATEWAY_CLI_BIND",
        default_value = "127.0.0.1:8176"
    )]
    pub cli_bind: SocketAddr,

    /// Public URL from which the webserver API is reachable
    #[arg(long = "api-addr", env = "FM_GATEWAY_API_ADDR")]
    pub api_addr: Option<SafeUrl>,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = "FM_GATEWAY_NETWORK")]
    pub network: Network,

    /// The username to use when connecting to bitcoind
    #[arg(long, env = "FM_BITCOIND_USERNAME")]
    pub bitcoind_username: Option<String>,

    /// The password to use when connecting to bitcoind
    #[arg(long, env = "FM_BITCOIND_PASSWORD")]
    pub bitcoind_password: Option<String>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    #[arg(long, env = "FM_BITCOIND_URL")]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = "FM_ESPLORA_URL")]
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
    #[arg(long = "default-routing-fees", env = "FM_DEFAULT_ROUTING_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = "FM_DEFAULT_TRANSACTION_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_transaction_fees: PaymentFee,
}

impl GatewayOpts {
    pub fn to_gateway_parameters(&self) -> anyhow::Result<GatewayParameters> {
        let versioned_api = self.api_addr.clone().map(|api_addr| {
            api_addr
                .join(V1_API_ENDPOINT)
                .expect("Could not join v1 api_addr")
        });

        Ok(GatewayParameters {
            api_bind: self.api_bind,
            cli_bind: self.cli_bind,
            versioned_api,
            network: self.network,
            default_routing_fees: self.default_routing_fees,
            default_transaction_fees: self.default_transaction_fees,
        })
    }
}

/// Runtime parameters derived from CLI args / environment variables.
#[derive(Debug)]
pub struct GatewayParameters {
    pub api_bind: SocketAddr,
    pub cli_bind: SocketAddr,
    pub versioned_api: Option<SafeUrl>,
    pub network: Network,
    pub default_routing_fees: PaymentFee,
    pub default_transaction_fees: PaymentFee,
}
