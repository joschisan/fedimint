use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use bitcoincore_rpc::RpcApi;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::Amount;
use fedimint_core::invite_code::InviteCode;
use fedimint_walletv2_client::WalletClientModule;
use tokio::process::Command;
use tracing::info;

fn find_binary(name: &str) -> PathBuf {
    let target_dir =
        std::env::var("CARGO_BUILD_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let profile = std::env::var("CARGO_PROFILE_DIR").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(format!("{target_dir}/{profile}/{name}"))
}

pub const BTC_RPC_PORT: u16 = 18443;
pub const GUARDIAN_BASE_PORT: u16 = 17000;
pub const PORTS_PER_GUARDIAN: u16 = 4;
pub const NUM_GUARDIANS: usize = 4;
pub const GW1_PORT: u16 = 28175;
pub const GW1_LN_PORT: u16 = 9735;
pub const GW1_METRICS_PORT: u16 = 29175;
pub const GW2_PORT: u16 = 28177;
pub const GW2_LN_PORT: u16 = 9736;
pub const GW2_METRICS_PORT: u16 = 29177;

pub const PASSWORD: &str = "theresnosecondbest";
const BTC_RPC_USER: &str = "bitcoin";
const BTC_RPC_PASS: &str = "bitcoin";

fn dummy_regtest_address() -> bitcoin::Address {
    "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .expect("valid address")
        .require_network(bitcoin::Network::Regtest)
        .expect("regtest address")
}

pub struct TestEnv {
    pub data_dir: tempfile::TempDir,
    pub bitcoind: bitcoincore_rpc::Client,
    pub invite_code: InviteCode,
    pub gw1_addr: String,
    pub gw2_addr: String,
    pub gw1_public: String,
    pub gw2_public: String,
    client_counter: AtomicU64,
}

impl TestEnv {
    pub async fn setup() -> anyhow::Result<Self> {
        let data_dir = tempfile::TempDir::new()?;
        let base = data_dir.path();
        info!("Test data directory: {}", base.display());

        let bitcoind = Self::connect_bitcoind().await?;

        for i in 0..NUM_GUARDIANS {
            start_fedimintd(base, i).await?;
        }

        info!("Running DKG...");
        run_dkg(base).await?;

        let peer0_dir = base.join("fedimintd-0");
        let invite_code_str = retry("read invite code", || async {
            tokio::fs::read_to_string(peer0_dir.join("invite-code"))
                .await
                .context("invite code not yet available")
        })
        .await?;
        let invite_code: InviteCode = invite_code_str.trim().parse()?;
        info!("Federation ready");

        start_gatewayd(base, "gw1", GW1_PORT, GW1_LN_PORT, GW1_METRICS_PORT).await?;
        start_gatewayd(base, "gw2", GW2_PORT, GW2_LN_PORT, GW2_METRICS_PORT).await?;

        // Admin API is on port+1, bound to localhost
        let gw1_addr = format!("http://127.0.0.1:{}", GW1_PORT + 1);
        let gw2_addr = format!("http://127.0.0.1:{}", GW2_PORT + 1);
        // Public API is on the base port (for LNv2 protocol)
        let gw1_public = format!("http://127.0.0.1:{GW1_PORT}");
        let gw2_public = format!("http://127.0.0.1:{GW2_PORT}");

        info!("Waiting for gateways...");
        retry("gw1 ready", || gateway_cli(&gw1_addr, &["info"])).await?;
        retry("gw2 ready", || gateway_cli(&gw2_addr, &["info"])).await?;
        info!("Gateways ready");

        info!("Connecting gateways to federation...");
        gateway_cli(&gw1_addr, &["federation", "join", invite_code_str.trim()]).await?;
        gateway_cli(&gw2_addr, &["federation", "join", invite_code_str.trim()]).await?;
        info!("Gateways connected");

        info!("Funding gateways and opening channel...");
        open_channel_between_gateways(&bitcoind, &gw1_addr, &gw2_addr).await?;
        info!("Channel opened");

        Ok(Self {
            data_dir,
            bitcoind,
            invite_code,
            gw1_addr,
            gw2_addr,
            gw1_public,
            gw2_public,
            client_counter: AtomicU64::new(0),
        })
    }

    async fn connect_bitcoind() -> anyhow::Result<bitcoincore_rpc::Client> {
        let url = format!("http://127.0.0.1:{BTC_RPC_PORT}/wallet/");
        let auth =
            bitcoincore_rpc::Auth::UserPass(BTC_RPC_USER.to_string(), BTC_RPC_PASS.to_string());
        let client = bitcoincore_rpc::Client::new(&url, auth)?;

        // Verify connection
        retry("connect to bitcoind", || async {
            tokio::task::block_in_place(|| client.get_blockchain_info())
                .context("bitcoind not reachable")
        })
        .await?;

        Ok(client)
    }

    pub async fn new_client(&self) -> anyhow::Result<ClientHandleArc> {
        let n = self.client_counter.fetch_add(1, Ordering::Relaxed);
        let db_path = self.data_dir.path().join(format!("client-{n}"));
        tokio::fs::create_dir_all(&db_path).await?;

        let db = fedimint_rocksdb::RocksDb::build(&db_path)
            .open()
            .await?
            .into();

        let mut builder = Client::builder().await?;
        builder.with_module(fedimint_mintv2_client::MintClientInit);
        builder.with_module(fedimint_walletv2_client::WalletClientInit);
        builder.with_module(fedimint_lnv2_client::LightningClientInit::default());

        let connectors = ConnectorRegistry::build_from_client_defaults()
            .bind()
            .await?;

        let mnemonic = Mnemonic::generate(12)?;
        let root_secret = RootSecret::StandardDoubleDerive(
            Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic),
        );

        let client = builder
            .preview(connectors, &self.invite_code)
            .await?
            .join(db, root_secret)
            .await
            .map(Arc::new)?;

        info!("Created client-{n}");
        Ok(client)
    }

    pub async fn new_cli_client_dir(&self) -> anyhow::Result<PathBuf> {
        let n = self.client_counter.fetch_add(1, Ordering::Relaxed);
        let dir = self.data_dir.path().join(format!("cli-client-{n}"));
        tokio::fs::create_dir_all(&dir).await?;

        fedimint_cli_raw(&[
            "--data-dir",
            dir.to_str().expect("valid path"),
            "join",
            &self.invite_code.to_string(),
        ])
        .await?;

        info!("Created cli-client-{n}");
        Ok(dir)
    }

    pub fn mine_blocks(&self, n: u64) {
        tokio::task::block_in_place(|| {
            self.bitcoind
                .generate_to_address(n, &dummy_regtest_address())
        })
        .expect("failed to mine blocks");
    }

    pub async fn pegin_gateway(&self, gw_addr: &str) -> anyhow::Result<()> {
        let fed_id = self.invite_code.federation_id().to_string();

        let value = gateway_cli(gw_addr, &["module", &fed_id, "walletv2", "receive"]).await?;

        let pegin_addr: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
            value.as_str().context("expected address string")?.parse()?;
        let pegin_addr = pegin_addr.assume_checked();

        tokio::task::block_in_place(|| self.bitcoind.generate_to_address(1, &pegin_addr))?;
        tokio::task::block_in_place(|| {
            self.bitcoind
                .generate_to_address(100, &dummy_regtest_address())
        })?;

        retry("gateway pegin balance", || {
            let gw_addr = gw_addr.to_string();
            let fed_id = fed_id.clone();
            async move {
                let info = gateway_cli(&gw_addr, &["info"]).await?;

                let feds = info["federations"]
                    .as_array()
                    .context("missing federations")?;

                let fed = feds
                    .iter()
                    .find(|f| f["federation_id"].as_str() == Some(&fed_id))
                    .context("federation not found")?;

                let balance = fed["balance_msat"].as_u64().context("missing balance")?;

                ensure!(balance > 0, "gateway balance is zero");
                Ok(())
            }
        })
        .await?;

        info!("Pegged in gateway {gw_addr}");
        Ok(())
    }

    pub async fn pegin(&self, client: &ClientHandleArc) -> anyhow::Result<()> {
        let walletv2 = client.get_first_module::<WalletClientModule>()?;
        let addr = walletv2.receive().await;

        tokio::task::block_in_place(|| self.bitcoind.generate_to_address(1, &addr))?;
        tokio::task::block_in_place(|| {
            self.bitcoind
                .generate_to_address(100, &dummy_regtest_address())
        })?;

        retry("pegin balance", || async {
            let balance = client.get_balance_for_btc().await?;
            ensure!(balance > Amount::ZERO, "balance is zero");
            Ok(())
        })
        .await?;

        info!("Pegged in (1 block to {addr})");
        Ok(())
    }
}

async fn start_fedimintd(base: &Path, peer_idx: usize) -> anyhow::Result<()> {
    let port_base = GUARDIAN_BASE_PORT + (peer_idx as u16 * PORTS_PER_GUARDIAN);
    let p2p_port = port_base;
    let api_port = port_base + 1;
    let ui_port = port_base + 2;
    let metrics_port = port_base + 3;

    let data_dir = base.join(format!("fedimintd-{peer_idx}"));
    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::File::create(base.join(format!("fedimintd-{peer_idx}.log")))?;

    Command::new(find_binary("fedimintd"))
        .env("FM_DATA_DIR", data_dir.to_str().unwrap())
        .env("FM_BITCOIN_NETWORK", "regtest")
        .env(
            "FM_BITCOIND_URL",
            format!("http://127.0.0.1:{BTC_RPC_PORT}"),
        )
        .env("FM_BITCOIND_USERNAME", BTC_RPC_USER)
        .env("FM_BITCOIND_PASSWORD", BTC_RPC_PASS)
        .env("FM_BIND_P2P", format!("127.0.0.1:{p2p_port}"))
        .env("FM_BIND_API", format!("127.0.0.1:{api_port}"))
        .env("FM_BIND_UI", format!("127.0.0.1:{ui_port}"))
        .env("FM_BIND_METRICS", format!("127.0.0.1:{metrics_port}"))
        .env("FM_P2P_URL", format!("fedimint://127.0.0.1:{p2p_port}"))
        .env("FM_API_URL", format!("ws://127.0.0.1:{api_port}"))
        .env("FM_ENABLE_MODULE_WALLETV2", "true")
        .env("FM_ENABLE_MODULE_WALLET", "false")
        .env("FM_ENABLE_MODULE_MINTV2", "true")
        .env("FM_ENABLE_MODULE_MINT", "false")
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start fedimintd-{peer_idx}"))?;

    info!("Started fedimintd-{peer_idx} on ports {p2p_port}-{metrics_port}");
    Ok(())
}

async fn start_gatewayd(
    base: &Path,
    name: &str,
    gw_port: u16,
    ln_port: u16,
    metrics_port: u16,
) -> anyhow::Result<()> {
    let data_dir = base.join(name);

    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::File::create(base.join(format!("{name}.log")))?;

    Command::new(find_binary("gatewayd"))
        .env("FM_GATEWAY_DATA_DIR", data_dir.to_str().unwrap())
        .env("FM_GATEWAY_LISTEN_ADDR", format!("127.0.0.1:{gw_port}"))
        .env("FM_GATEWAY_API_ADDR", format!("http://127.0.0.1:{gw_port}"))
        .env("FM_PORT_LDK", ln_port.to_string())
        .env(
            "FM_GATEWAY_METRICS_LISTEN_ADDR",
            format!("127.0.0.1:{metrics_port}"),
        )
        .env("FM_GATEWAY_SKIP_WAIT_FOR_SYNC", "1")
        .env("FM_GATEWAY_NETWORK", "regtest")
        .env("FM_DEFAULT_ROUTING_FEES", "0,0")
        .env("FM_LDK_ALIAS", name)
        .env("FM_GATEWAY_SKIP_SETUP", "true")
        .env(
            "FM_BITCOIND_URL",
            format!("http://127.0.0.1:{BTC_RPC_PORT}"),
        )
        .env("FM_BITCOIND_USERNAME", BTC_RPC_USER)
        .env("FM_BITCOIND_PASSWORD", BTC_RPC_PASS)
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start {name}"))?;

    info!("Started {name} on port {gw_port}");
    Ok(())
}

async fn run_dkg(base: &Path) -> anyhow::Result<()> {
    let mut endpoints = BTreeMap::new();
    for i in 0..NUM_GUARDIANS {
        let api_port = GUARDIAN_BASE_PORT + (i as u16 * PORTS_PER_GUARDIAN) + 1;
        endpoints.insert(i, format!("ws://127.0.0.1:{api_port}"));
    }

    // Wait for all guardians to be ready
    for (peer, endpoint) in &endpoints {
        retry(&format!("fedimintd-{peer} setup status"), || {
            let endpoint = endpoint.clone();
            async move {
                let result = fedimint_cli_raw(&[
                    "--password",
                    PASSWORD,
                    "admin",
                    "setup",
                    &endpoint,
                    "status",
                ])
                .await?;
                let status = result.trim();
                ensure!(
                    status.contains("AwaitingLocalParams"),
                    "Unexpected status: {status}"
                );
                Ok(())
            }
        })
        .await?;
    }
    info!("All guardians awaiting local params");

    // Set local params: peer 0 is leader, rest are followers
    let mut connection_infos = BTreeMap::new();
    for (peer, endpoint) in &endpoints {
        let mut args = vec![
            "--password".to_string(),
            PASSWORD.to_string(),
            "admin".to_string(),
            "setup".to_string(),
            endpoint.clone(),
            "set-local-params".to_string(),
            format!("Guardian {peer}"),
        ];

        if *peer == 0 {
            args.push("--federation-name".to_string());
            args.push("Test Federation".to_string());
            args.push("--federation-size".to_string());
            args.push(NUM_GUARDIANS.to_string());
        }

        let output = fedimint_cli_raw(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await?;
        let info: String = serde_json::from_str(output.trim())?;
        connection_infos.insert(*peer, info);
    }
    info!("Local params set for all guardians");

    // Exchange peer connection info
    for (peer, info) in &connection_infos {
        for (other_peer, endpoint) in &endpoints {
            if other_peer == peer {
                continue;
            }
            fedimint_cli_raw(&[
                "--password",
                PASSWORD,
                "admin",
                "setup",
                endpoint,
                "add-peer",
                info,
            ])
            .await?;
        }
    }
    info!("Peer info exchanged");

    // Start DKG on all peers
    for endpoint in endpoints.values() {
        fedimint_cli_raw(&[
            "--password",
            PASSWORD,
            "admin",
            "setup",
            endpoint,
            "start-dkg",
        ])
        .await?;
    }

    // Wait for invite code to appear
    let peer0_dir = base.join("fedimintd-0");
    retry("await invite-code file", || async {
        tokio::fs::read_to_string(peer0_dir.join("invite-code"))
            .await
            .context("invite code not yet written")
    })
    .await?;

    info!("DKG complete");
    Ok(())
}

pub async fn fedimint_cli_raw(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(find_binary("fedimint-cli"))
        .args(args)
        .output()
        .await
        .context("Failed to run fedimint-cli")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "fedimint-cli {:?} failed:\nstdout: {stdout}\nstderr: {stderr}",
            args
        );
    }

    Ok(String::from_utf8(output.stdout)?)
}

pub async fn gateway_cli(gw_addr: &str, args: &[&str]) -> anyhow::Result<serde_json::Value> {
    let mut full_args = vec!["-a", gw_addr];
    full_args.extend(args);

    let output = Command::new(find_binary("gateway-cli"))
        .args(&full_args)
        .output()
        .await
        .context("Failed to run gateway-cli")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "gateway-cli {} failed:\nstdout: {stdout}\nstderr: {stderr}",
            args.join(" ")
        );
    }

    let stdout = String::from_utf8(output.stdout)?;
    if stdout.trim().is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::from_str(stdout.trim()).context("Failed to parse gateway-cli JSON output")
    }
}

async fn open_channel_between_gateways(
    bitcoind: &bitcoincore_rpc::Client,
    gw1_addr: &str,
    gw2_addr: &str,
) -> anyhow::Result<()> {
    let gw2_info = gateway_cli(gw2_addr, &["info"]).await?;

    for gw_addr in [gw1_addr, gw2_addr] {
        let addr_json = gateway_cli(gw_addr, &["ldk", "onchain", "receive"]).await?;
        let funding_addr = addr_json.as_str().context("missing address string")?;

        let addr: bitcoin::Address<bitcoin::address::NetworkUnchecked> = funding_addr.parse()?;
        let addr = addr.assume_checked();
        tokio::task::block_in_place(|| bitcoind.generate_to_address(1, &addr))?;
        tokio::task::block_in_place(|| {
            bitcoind.generate_to_address(100, &dummy_regtest_address())
        })?;
    }

    let target_height = tokio::task::block_in_place(|| bitcoind.get_block_count())? - 1;
    for gw_addr in [gw1_addr, gw2_addr] {
        retry("gateway sync", || {
            let gw_addr = gw_addr.to_string();
            async move {
                let info = gateway_cli(&gw_addr, &["info"]).await?;
                let height = info["lightning_info"]["connected"]["block_height"]
                    .as_u64()
                    .context("missing block_height")?;
                ensure!(
                    height >= target_height,
                    "not synced: {height} < {target_height}"
                );
                Ok(())
            }
        })
        .await?;
    }

    let gw2_pubkey = gw2_info["lightning_info"]["connected"]["public_key"]
        .as_str()
        .context("missing gw2 public_key")?;

    let gw2_ln_addr = format!("127.0.0.1:{GW2_LN_PORT}");

    gateway_cli(
        gw1_addr,
        &[
            "ldk",
            "channel",
            "open",
            gw2_pubkey,
            &gw2_ln_addr,
            "10000000",
            "--push-amount-sats",
            "5000000",
        ],
    )
    .await?;

    // Mine to confirm channel
    tokio::task::block_in_place(|| bitcoind.generate_to_address(10, &dummy_regtest_address()))?;

    // Wait for channel to be active on both sides
    for gw_addr in [gw1_addr, gw2_addr] {
        retry("channel active", || {
            let gw_addr = gw_addr.to_string();
            async move {
                let channels = gateway_cli(&gw_addr, &["ldk", "channel", "list"]).await?;
                let channels = channels.as_array().context("channels not an array")?;
                ensure!(
                    channels
                        .iter()
                        .any(|c| c["is_usable"].as_bool() == Some(true)),
                    "no active channels yet"
                );
                Ok(())
            }
        })
        .await?;
    }

    Ok(())
}

pub async fn retry<F, Fut, T>(name: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    for i in 0..60 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if i == 59 {
                    return Err(e).context(format!("retry '{name}' exhausted after 60 attempts"));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    unreachable!()
}
