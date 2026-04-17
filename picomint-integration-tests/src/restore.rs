use anyhow::{Result, ensure};
use tracing::info;

use crate::cli;
use crate::env::{TestEnv, retry};

/// Poll until guardian `peer_idx` reports a non-zero finalized session
/// count — proves it's participating in consensus.
async fn retry_non_zero_session_count(env: &TestEnv, peer_idx: usize) -> Result<u64> {
    let data_dir = env.data_dir.join(format!("picomintd-{peer_idx}"));

    retry(&format!("server-{peer_idx} session count > 0"), || {
        let data_dir = data_dir.clone();
        async move {
            let count = cli::picomintd_session_count(&data_dir)?;
            ensure!(count > 0, "session count still 0");
            Ok(count)
        }
    })
    .await
}

pub async fn run_test(env: &TestEnv) -> Result<()> {
    let peer_idx = 0;
    let data_dir = env.data_dir.join(format!("picomintd-{peer_idx}"));

    info!("waiting for guardian-{peer_idx} to finalize a session");
    retry_non_zero_session_count(env, peer_idx).await?;

    info!("backing up config");
    let original_cfg = cli::picomintd_config(&data_dir)?;
    let backup_path = env.data_dir.join("config.json");
    std::fs::write(&backup_path, serde_json::to_vec_pretty(&original_cfg)?)?;

    info!("killing guardian-{peer_idx} and wiping its data dir");
    env.wipe_guardian(peer_idx).await?;

    info!("restarting guardian-{peer_idx} (fresh)");
    env.restart_guardian(peer_idx).await?;

    retry(&format!("server-{peer_idx} in setup mode"), || async {
        cli::picomintd_setup_status(&data_dir)
    })
    .await?;

    info!("uploading saved config");
    cli::picomintd_setup_restore(&data_dir, &backup_path)?;

    info!("waiting for guardian-{peer_idx} to rejoin consensus");
    retry_non_zero_session_count(env, peer_idx).await?;

    info!("verifying restored config matches original");
    let restored_cfg = cli::picomintd_config(&data_dir)?;
    ensure!(
        restored_cfg == original_cfg,
        "restored config does not match original"
    );

    info!("restore test OK");
    Ok(())
}
