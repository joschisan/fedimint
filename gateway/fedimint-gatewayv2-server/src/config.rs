use std::net::SocketAddr;
use std::path::PathBuf;

use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::util::SafeUrl;
use fedimint_lnv2_common::gateway_api::PaymentFee;

use super::envs;
use crate::envs::{FM_BITCOIND_URL_ENV, FM_ESPLORA_URL_ENV};

/// Command line parameters for starting the gateway. `gatewaydv2` is LDK + LNv2
/// only, so there is no lightning-backend subcommand.
#[derive(Parser)]
#[command(version)]
#[command(group(
    ArgGroup::new("bitcoin_rpc")
        .required(true)
        .multiple(true)
        .args(["bitcoind_url", "esplora_url"])
))]
pub struct GatewayOpts {
    /// Path to folder containing gateway config and data files
    #[arg(long = "data-dir", env = envs::FM_DATA_DIR_ENV)]
    pub data_dir: PathBuf,

    /// Address the gateway's API webserver (the LNv2 routes) listens on.
    #[arg(long = "api-addr", env = envs::FM_API_ADDR_ENV, default_value = "0.0.0.0:8080")]
    pub api_addr: SocketAddr,

    /// Address and port for the LDK node's lightning P2P (BOLT) interface.
    #[arg(long = "ldk-addr", env = envs::FM_LDK_ADDR_ENV, default_value = "0.0.0.0:9735")]
    pub ldk_addr: SocketAddr,

    /// The LDK node's advertised alias.
    #[arg(long = "ldk-alias", env = envs::FM_LDK_ALIAS_ENV, default_value = "")]
    pub ldk_alias: String,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = envs::FM_NETWORK_ENV, default_value = "bitcoin")]
    pub network: Network,

    /// Bitcoind RPC URL with credentials embedded in the URL, e.g.
    /// `http://user:pass@127.0.0.1:8332`.
    #[arg(long, env = FM_BITCOIND_URL_ENV)]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = FM_ESPLORA_URL_ENV)]
    pub esplora_url: Option<SafeUrl>,

    /// The default routing fees that are applied to new federations
    #[arg(long = "default-routing-fees", env = envs::FM_DEFAULT_ROUTING_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = envs::FM_DEFAULT_TRANSACTION_FEES_ENV, default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_transaction_fees: PaymentFee,
}
