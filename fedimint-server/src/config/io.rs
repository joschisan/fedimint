use std::fmt::Display;
use std::fs;
use std::path::Path;

use fedimint_server_core::ServerModuleInitRegistry;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::ServerConfig;

/// Client configuration file
pub const CLIENT_CONFIG: &str = "client";

/// Server private keys file
pub const PRIVATE_CONFIG: &str = "private";

/// Server locally configurable file
pub const LOCAL_CONFIG: &str = "local";

/// Server consensus-only configurable file
pub const CONSENSUS_CONFIG: &str = "consensus";

/// Client connection string file
pub const CLIENT_INVITE_CODE_FILE: &str = "invite-code";

/// Database file name
pub const DB_FILE: &str = "database";

pub const JSON_EXT: &str = "json";

/// Reads the server config from plaintext JSON files.
pub fn read_server_config(path: &Path) -> anyhow::Result<ServerConfig> {
    Ok(ServerConfig {
        consensus: plaintext_json_read(&path.join(CONSENSUS_CONFIG))?,
        local: plaintext_json_read(&path.join(LOCAL_CONFIG))?,
        private: plaintext_json_read(&path.join(PRIVATE_CONFIG))?,
    })
}

/// Reads a plaintext json file into a struct
fn plaintext_json_read<T: Serialize + DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let string = fs::read_to_string(path.with_extension(JSON_EXT))?;
    Ok(serde_json::from_str(&string)?)
}

/// Writes the server into configuration files as plaintext JSON.
pub fn write_server_config(
    server: &ServerConfig,
    path: &Path,
    module_config_gens: &ServerModuleInitRegistry,
    api_secret: Option<String>,
) -> anyhow::Result<()> {
    let client_config = server.consensus.to_client_config(module_config_gens)?;
    plaintext_json_write(&server.local, &path.join(LOCAL_CONFIG))?;
    plaintext_json_write(&server.consensus, &path.join(CONSENSUS_CONFIG))?;
    plaintext_display_write(
        &server.get_invite_code(api_secret),
        &path.join(CLIENT_INVITE_CODE_FILE),
    )?;
    plaintext_json_write(&client_config, &path.join(CLIENT_CONFIG))?;
    plaintext_json_write(&server.private, &path.join(PRIVATE_CONFIG))
}

/// Writes struct into a plaintext json file
fn plaintext_json_write<T: Serialize + DeserializeOwned>(
    obj: &T,
    path: &Path,
) -> anyhow::Result<()> {
    let file = fs::File::options()
        .create_new(true)
        .write(true)
        .open(path.with_extension(JSON_EXT))?;

    serde_json::to_writer_pretty(file, obj)?;
    Ok(())
}

fn plaintext_display_write<T: Display>(obj: &T, path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = fs::File::options()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(obj.to_string().as_bytes())?;
    Ok(())
}
