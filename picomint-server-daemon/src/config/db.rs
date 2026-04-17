use picomint_redb::table;
use picomint_redb::Database;

use crate::config::ServerConfig;

picomint_redb::consensus_value!(ServerConfig);

table!(
    SERVER_CONFIG,
    () => ServerConfig,
    "server-config",
);

pub async fn load_server_config(db: &Database) -> Option<ServerConfig> {
    db.begin_read().await.get(&SERVER_CONFIG, &())
}

pub async fn store_server_config(db: &Database, cfg: &ServerConfig) {
    let tx = db.begin_write().await;
    assert!(
        tx.insert(&SERVER_CONFIG, &(), cfg).is_none(),
        "Server config already present in database"
    );
    tx.commit().await;
}
