use picomint_core::config::ConsensusConfig;
use picomint_redb::table;

table!(
    CLIENT_CONFIG,
    () => ConsensusConfig,
    "client-config",
);
