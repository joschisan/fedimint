/// Environment variable that specifies the directory of the gateway's database.
pub const FM_DATA_DIR_ENV: &str = "FM_DATA_DIR";

/// Environment variable that specifies the address the gateway's API webserver
/// (the LNv2 routes) should listen on.
pub const FM_API_ADDR_ENV: &str = "FM_API_ADDR";

/// Environment variable that specifies the address and port for the LDK node's
/// lightning P2P (BOLT) interface.
pub const FM_LDK_ADDR_ENV: &str = "FM_LDK_ADDR";

/// Environment variable that specifies the LDK node's advertised alias.
pub const FM_LDK_ALIAS_ENV: &str = "FM_LDK_ALIAS";

/// Environment variable that specifies that Bitcoin network that the gateway
/// should use. Must match the network of the Lightning node.
pub const FM_NETWORK_ENV: &str = "FM_NETWORK";

/// The URL to use when connecting to a bitcoin node over RPC, with credentials
/// embedded in the URL (e.g. `http://user:pass@127.0.0.1:8332`).
pub const FM_BITCOIND_URL_ENV: &str = "FM_BITCOIND_URL";

/// The URL to use when connecting to an Esplora server for bitcoin blockchain
/// data
pub const FM_ESPLORA_URL_ENV: &str = "FM_ESPLORA_URL";

/// Environment variable for customizing the default routing fees
pub const FM_DEFAULT_ROUTING_FEES_ENV: &str = "FM_DEFAULT_ROUTING_FEES";

/// Environment variable for customizing the default transaction fees
pub const FM_DEFAULT_TRANSACTION_FEES_ENV: &str = "FM_DEFAULT_TRANSACTION_FEES";
