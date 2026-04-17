use anyhow::Result;
use clap::Parser;
use devimint::cmd;
use devimint::federation::Federation;
use devimint::util::almost_equal;
use fedimint_logging::LOG_DEVIMINT;
use rand::Rng;
use tracing::info;

#[derive(Debug, Parser)]
enum Cmd {
    Restore,
    RecoveryV1,
    RecoveryV2,
    Sanity,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cmd::parse() {
        Cmd::Restore => restore().await,
        Cmd::RecoveryV1 => mint_recovery_test().await,
        Cmd::RecoveryV2 => mint_recovery_test().await,
        Cmd::Sanity => sanity().await,
    }
}

async fn restore() -> anyhow::Result<()> {
    devimint::run_devfed_test()
        .call(|fed, _process_mgr| async move {
            let fed = fed.fed().await?;

            test_restore_gap_test(fed).await?;
            Ok(())
        })
        .await
}

pub async fn test_restore_gap_test(fed: &Federation) -> Result<()> {
    let client = fed.new_joined_client("restore-gap-test").await?;
    const PEGIN_SATS: u64 = 300000;
    fed.pegin_client(PEGIN_SATS, &client).await?;

    for i in 0..20 {
        let gap = rand::thread_rng().gen_range(0..20);
        info!(target: LOG_DEVIMINT, gap, "Gap");
        cmd!(
            client,
            "dev",
            "advance-note-idx",
            "--amount",
            "1024msat",
            "--count",
            // we are not guaranteed to use a 1024 note on every payment,
            // so create some random small gaps, so it's very unlikely we
            // would cross the default gap limit accidentally
            &gap.to_string()
        )
        .run()
        .await?;

        // We need to get the balance of the client to know how much to reissue, due to
        // the mint base fees it decreases slightly every time we reissue.
        let notes = cmd!(client, "info").out_json().await?;
        let balance = notes["total_amount_msat"].as_u64().unwrap();

        let reissure_amount = if i % 2 == 0 {
            // half of the time, reissue everything
            balance
        } else {
            // other half, random amount
            rand::thread_rng().gen_range(10..(balance))
        };
        info!(target: LOG_DEVIMINT, i, reissure_amount, "Reissue");

        let notes = cmd!(client, "spend", reissure_amount)
            .out_json()
            .await?
            .get("notes")
            .expect("Output didn't contain e-cash notes")
            .as_str()
            .unwrap()
            .to_owned();

        // Test we can reissue our own notes
        cmd!(client, "reissue", notes).out_json().await?;
    }

    let secret = cmd!(client, "print-secret").out_json().await?["secret"]
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap();

    let pre_notes = cmd!(client, "info").out_json().await?;

    let pre_balance = pre_notes["total_amount_msat"].as_u64().unwrap();

    info!(target: LOG_DEVIMINT, %pre_notes, pre_balance, "State before backup");

    // we need to have some funds
    assert!(0 < pre_balance);

    // without existing backup
    {
        let client =
            devimint::federation::Client::create("restore-gap-test-without-backup").await?;
        let _ = cmd!(
            client,
            "restore",
            "--mnemonic",
            &secret,
            "--invite-code",
            fed.invite_code()?
        )
        .out_json()
        .await?;

        let _ = cmd!(client, "dev", "wait-complete").out_json().await?;
        let post_notes = cmd!(client, "info").out_json().await?;
        let post_balance = post_notes["total_amount_msat"].as_u64().unwrap();
        info!(target: LOG_DEVIMINT, %post_notes, post_balance, "State after backup");
        assert_eq!(pre_balance, post_balance);
        assert_eq!(pre_notes, post_notes);
    }

    Ok(())
}

/// Test that mint recovery works correctly in various scenarios.
///
/// The V1 variant should be run with `FM_FORCE_V1_MINT_RECOVERY=1` to
/// force the legacy session-based recovery path which reissues recovered
/// ecash.
///
/// Regression test for <https://github.com/fedimint/fedimint/issues/8004>
async fn mint_recovery_test() -> anyhow::Result<()> {
    devimint::run_devfed_test()
        .call(|dev_fed, _process_mgr| async move {
            let fed = dev_fed.fed().await?;

            const PEGIN_SATS: u64 = 100_000;

            info!(target: LOG_DEVIMINT, "### Test mint recovery with backup");
            {
                let client = fed.new_joined_client("mint-recovery-backup").await?;
                fed.pegin_client(PEGIN_SATS, &client).await?;

                let pre_balance = client.balance().await?;
                info!(target: LOG_DEVIMINT, pre_balance, "Balance before backup");
                assert!(pre_balance > 0);

                // Upload backup so the federation stores spendable notes
                cmd!(client, "backup").run().await?;

                // Restore triggers recovery; with V1 this exercises the
                // finalize_dbtx reissuance codepath
                let restored = client
                    .new_restored("mint-restored-with-backup", fed.invite_code()?)
                    .await?;
                cmd!(restored, "dev", "wait-complete").out_json().await?;

                let post_balance = restored.balance().await?;
                info!(target: LOG_DEVIMINT, post_balance, "Balance after recovery with backup");
                almost_equal(pre_balance, post_balance, 100_000).unwrap();
            }

            info!(target: LOG_DEVIMINT, "### Test mint recovery without backup");
            {
                let client = fed.new_joined_client("mint-recovery-no-backup").await?;
                fed.pegin_client(PEGIN_SATS, &client).await?;

                let pre_balance = client.balance().await?;
                assert!(pre_balance > 0);

                // Restore without ever calling backup — recovery from history only
                let restored = client
                    .new_restored("mint-restored-no-backup", fed.invite_code()?)
                    .await?;
                cmd!(restored, "dev", "wait-complete").out_json().await?;

                let post_balance = restored.balance().await?;
                info!(target: LOG_DEVIMINT, post_balance, "Balance after recovery without backup");
                almost_equal(pre_balance, post_balance, 100_000).unwrap();
            }

            info!(target: LOG_DEVIMINT, "### Test mint recovery after spend+reissue activity");
            {
                let client = fed
                    .new_joined_client("mint-recovery-after-activity")
                    .await?;
                fed.pegin_client(PEGIN_SATS, &client).await?;

                // Do several rounds of spend-to-self to churn note denominations
                for i in 0..3 {
                    let balance = client.balance().await?;
                    let spend_amount = balance / 3;

                    let notes = cmd!(client, "spend", spend_amount)
                        .out_json()
                        .await?
                        .get("notes")
                        .expect("Output didn't contain e-cash notes")
                        .as_str()
                        .unwrap()
                        .to_owned();

                    cmd!(client, "reissue", notes).out_json().await?;
                    info!(target: LOG_DEVIMINT, i, spend_amount, "Spent and reissued to self");
                }

                let pre_balance = client.balance().await?;
                info!(target: LOG_DEVIMINT, pre_balance, "Balance after activity");
                assert!(pre_balance > 0);

                cmd!(client, "backup").run().await?;

                let restored = client
                    .new_restored("mint-restored-after-activity", fed.invite_code()?)
                    .await?;
                cmd!(restored, "dev", "wait-complete").out_json().await?;

                let post_balance = restored.balance().await?;
                info!(target: LOG_DEVIMINT, post_balance, "Balance after recovery post-activity");
                almost_equal(pre_balance, post_balance, 100_000).unwrap();
            }

            info!(target: LOG_DEVIMINT, "### Test mint recovery with post-backup activity");
            {
                let client = fed
                    .new_joined_client("mint-recovery-post-backup")
                    .await?;
                fed.pegin_client(PEGIN_SATS, &client).await?;

                // Backup while we still have the original notes
                cmd!(client, "backup").run().await?;

                // Spend and reissue after the backup — these note changes
                // won't be in the backup, only in the federation history
                let balance = client.balance().await?;
                let spend_amount = balance / 2;
                let notes = cmd!(client, "spend", spend_amount)
                    .out_json()
                    .await?
                    .get("notes")
                    .expect("Output didn't contain e-cash notes")
                    .as_str()
                    .unwrap()
                    .to_owned();
                cmd!(client, "reissue", notes).out_json().await?;

                let pre_balance = client.balance().await?;
                info!(target: LOG_DEVIMINT, pre_balance, "Balance after post-backup activity");

                let restored = client
                    .new_restored("mint-restored-post-backup", fed.invite_code()?)
                    .await?;
                cmd!(restored, "dev", "wait-complete").out_json().await?;

                let post_balance = restored.balance().await?;
                info!(target: LOG_DEVIMINT, post_balance, "Balance after recovery with post-backup activity");
                almost_equal(pre_balance, post_balance, 100_000).unwrap();
            }

            Ok(())
        })
        .await
}

async fn sanity() -> anyhow::Result<()> {
    devimint::run_devfed_test()
        .call(|_fed, _process_mgr| async move { Ok(()) })
        .await
}
