use std::net::SocketAddr;
use std::path::PathBuf;

use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::util::SafeUrl;
use fedimint_gateway_common::LightningMode;
use fedimint_lnv2_common::gateway_api::PaymentFee;

use super::envs;
use crate::envs::{
    FM_BITCOIND_PASSWORD_ENV, FM_BITCOIND_URL_ENV, FM_BITCOIND_USERNAME_ENV, FM_ESPLORA_URL_ENV,
};

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
    #[clap(subcommand)]
    pub mode: LightningMode,

    /// Path to folder containing gateway config and data files
    #[arg(long = "data-dir", env = envs::FM_GATEWAY_DATA_DIR_ENV)]
    pub data_dir: PathBuf,

    /// Gateway webserver listen address
    #[arg(long = "listen", env = envs::FM_GATEWAY_LISTEN_ADDR_ENV)]
    listen: SocketAddr,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = envs::FM_GATEWAY_NETWORK_ENV)]
    network: Network,

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

    /// The default routing fees that are applied to new federations
    #[arg(long = "default-routing-fees", env = envs::FM_DEFAULT_ROUTING_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = envs::FM_DEFAULT_TRANSACTION_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    default_transaction_fees: PaymentFee,
}

impl GatewayOpts {
    /// Converts the command line parameters into a helper struct the Gateway
    /// uses to store runtime parameters.
    pub fn to_gateway_parameters(&self) -> anyhow::Result<GatewayParameters> {
        Ok(GatewayParameters {
            listen: self.listen,
            network: self.network,
            default_routing_fees: self.default_routing_fees,
            default_transaction_fees: self.default_transaction_fees,
        })
    }
}

/// `GatewayParameters` is a helper struct that can be derived from
/// `GatewayOpts` that holds the CLI or environment variables that are specified
/// by the user.
#[derive(Debug)]
pub struct GatewayParameters {
    pub listen: SocketAddr,
    pub network: Network,
    pub default_routing_fees: PaymentFee,
    pub default_transaction_fees: PaymentFee,
}
