use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use bitcoin::Network;
use bitcoincore_rpc::RpcApi;
use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use picomint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use picomint_client::secret::RootSecretStrategy;
use picomint_client::{Client, ClientHandleArc, RootSecret};
use picomint_core::Amount;
use picomint_core::invite_code::InviteCode;
use picomint_wallet_client::WalletClientModule;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::block_in_place;
use tracing::info;

use crate::cli;

pub const BTC_RPC_PORT: u16 = 18443;
pub const GUARDIAN_BASE_PORT: u16 = 17000;
pub const PORTS_PER_GUARDIAN: u16 = 5;
pub const NUM_GUARDIANS: usize = 4;
pub const GW_PORT: u16 = 28175;
pub const GW_LN_PORT: u16 = 9735;
pub const TEST_LDK_PORT: u16 = 9736;

const BTC_RPC_USER: &str = "bitcoin";
const BTC_RPC_PASS: &str = "bitcoin";

fn dummy_address() -> bitcoin::Address {
    "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .expect("valid address")
        .require_network(bitcoin::Network::Regtest)
        .expect("regtest address")
}

pub struct TestEnv {
    pub ldk_node: Arc<ldk_node::Node>,
    pub data_dir: std::path::PathBuf,
    pub bitcoind: bitcoincore_rpc::Client,
    pub invite_code: InviteCode,
    pub gw_data_dir: std::path::PathBuf,
    pub gw_public: String,
    pub endpoint: Endpoint,
    pub client_counter: AtomicU64,
    /// One per guardian, indexed by peer id. `None` once we've killed it.
    pub guardian_processes: Mutex<Vec<Option<Child>>>,
}

impl TestEnv {
    pub fn setup(runtime: Arc<tokio::runtime::Runtime>) -> anyhow::Result<(Self, ClientHandleArc)> {
        let data_dir = tempfile::TempDir::new()?.keep();
        let base = data_dir.as_path();
        info!("Test data directory: {}", base.display());

        let bitcoind = Self::connect_bitcoind(&runtime)?;

        // Fund bitcoind's own wallet so peg-ins can be regular (non-coinbase)
        // transactions — avoids the 100-block coinbase maturity wait.
        let funding_addr = bitcoind
            .get_new_address(None, None)?
            .require_network(bitcoin::Network::Regtest)?;
        bitcoind.generate_to_address(101, &funding_addr)?;

        let mut guardian_processes = Vec::with_capacity(NUM_GUARDIANS);
        for i in 0..NUM_GUARDIANS {
            let child = runtime.block_on(start_picomintd(base, i))?;
            guardian_processes.push(Some(child));
        }

        info!("Running DKG...");
        let peer_data_dirs: Vec<_> = (0..NUM_GUARDIANS)
            .map(|i| base.join(format!("picomintd-{i}")))
            .collect();
        runtime.block_on(run_dkg(&peer_data_dirs))?;

        let peer0_data_dir = peer_data_dirs[0].clone();
        let invite_code_str = runtime.block_on(retry("fetch invite code", || async {
            Ok(cli::picomintd_invite(&peer0_data_dir)?.invite_code)
        }))?;
        let invite_code: InviteCode = invite_code_str.trim().parse()?;
        info!("Federation ready");

        // Bind the iroh endpoint now so we can start building the first client
        // concurrently with the rest of setup — address grinding is the
        // slowest part of client construction and benefits from overlapping
        // with gateway/LDK bring-up.
        let endpoint = runtime.block_on(async { Endpoint::builder(N0).bind().await })?;

        let client_counter = AtomicU64::new(0);
        let client_send = runtime.block_on(build_client(
            endpoint.clone(),
            invite_code.clone(),
            data_dir.clone(),
            client_counter.fetch_add(1, Ordering::Relaxed),
        ))?;

        runtime.block_on(start_gatewayd(base, "gw", GW_PORT, GW_LN_PORT))?;

        let gw_data_dir = base.join("gw");
        // Public API is on the base port
        let gw_public = format!("http://127.0.0.1:{GW_PORT}");

        info!("Waiting for gateway...");
        runtime.block_on(retry("gw ready", || async {
            cli::gatewayd_info(&gw_data_dir).map(|_| ())
        }))?;
        info!("Gateway ready");

        info!("Connecting gateway to federation...");
        cli::gatewayd_federation_join(&gw_data_dir, invite_code_str.trim())?;
        info!("Gateway connected");

        info!("Building freestanding LDK node...");
        let ldk_node = build_ldk_node(base, runtime.clone())?;
        info!("LDK node built: {}", ldk_node.node_id());

        info!("Funding gateway and opening channel to LDK node...");
        runtime.block_on(open_channel(&bitcoind, &gw_data_dir, &ldk_node))?;
        info!("Channel opened");

        Ok((
            Self {
                ldk_node,
                data_dir,
                bitcoind,
                invite_code,
                gw_data_dir,
                gw_public,
                endpoint,
                client_counter,
                guardian_processes: Mutex::new(guardian_processes),
            },
            client_send,
        ))
    }

    /// SIGKILL a single guardian process and delete its data directory,
    /// simulating a total disk loss. Use `restart_guardian` to bring it
    /// back up against an empty data dir.
    pub async fn wipe_guardian(&self, peer_idx: usize) -> anyhow::Result<()> {
        let mut procs = self.guardian_processes.lock().await;
        if let Some(mut child) = procs[peer_idx].take() {
            child.kill().await?;
            child.wait().await?;
        }
        drop(procs);

        let data_dir = self.data_dir.join(format!("picomintd-{peer_idx}"));
        tokio::fs::remove_dir_all(&data_dir).await?;
        Ok(())
    }

    /// Spawn a fresh daemon for `peer_idx` against its existing data dir.
    pub async fn restart_guardian(&self, peer_idx: usize) -> anyhow::Result<()> {
        let child = start_picomintd(&self.data_dir, peer_idx).await?;
        self.guardian_processes.lock().await[peer_idx] = Some(child);
        Ok(())
    }

    fn connect_bitcoind(
        runtime: &tokio::runtime::Runtime,
    ) -> anyhow::Result<bitcoincore_rpc::Client> {
        let url = format!("http://127.0.0.1:{BTC_RPC_PORT}/wallet/");
        let auth =
            bitcoincore_rpc::Auth::UserPass(BTC_RPC_USER.to_string(), BTC_RPC_PASS.to_string());
        let client = bitcoincore_rpc::Client::new(&url, auth)?;

        // Verify connection
        runtime.block_on(retry("connect to bitcoind", || async {
            client
                .get_blockchain_info()
                .context("bitcoind not reachable")
        }))?;

        Ok(client)
    }

    pub async fn new_client(&self) -> anyhow::Result<ClientHandleArc> {
        let n = self.client_counter.fetch_add(1, Ordering::Relaxed);
        build_client(
            self.endpoint.clone(),
            self.invite_code.clone(),
            self.data_dir.clone(),
            n,
        )
        .await
    }

    pub fn mine_blocks(&self, n: u64) {
        block_in_place(|| self.bitcoind.generate_to_address(n, &dummy_address())).unwrap();
    }

    pub fn send_to_address(
        &self,
        addr: &bitcoin::Address,
        amount: bitcoin::Amount,
    ) -> anyhow::Result<bitcoin::Txid> {
        Ok(block_in_place(|| {
            self.bitcoind
                .send_to_address(addr, amount, None, None, None, None, None, None)
        })?)
    }

    pub async fn pegin(
        &self,
        client: &ClientHandleArc,
        amount: bitcoin::Amount,
    ) -> anyhow::Result<()> {
        let wallet = client.get_first_module::<WalletClientModule>()?;
        let addr = wallet.receive().await;
        info!(%addr, "Pegin address ready");

        let txid = self.send_to_address(&addr, amount)?;

        retry("pegin tx in mempool", || async {
            block_in_place(|| self.bitcoind.get_mempool_entry(&txid))
                .map(|_| ())
                .context("pegin tx not in mempool yet")
        })
        .await?;

        self.mine_blocks(10);

        retry("pegin balance", || async {
            let balance = client.get_balance().await?;
            ensure!(balance > Amount::ZERO, "balance is zero");
            Ok(())
        })
        .await?;

        info!("Pegged in to {addr}");
        Ok(())
    }
}

async fn build_client(
    endpoint: Endpoint,
    invite_code: InviteCode,
    data_dir: std::path::PathBuf,
    n: u64,
) -> anyhow::Result<ClientHandleArc> {
    let db_dir = data_dir.join(format!("client-{n}"));
    tokio::fs::create_dir_all(&db_dir).await?;

    let db = picomint_redb::Database::open(db_dir.join("database.redb")).await?;

    let builder = Client::builder().await?;

    let connectors = endpoint;

    let mnemonic = Mnemonic::generate(12)?;
    let root_secret =
        RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic));

    let client = builder
        .preview(connectors, &invite_code)
        .await?
        .join(db, root_secret)
        .await
        .map(Arc::new)?;

    info!("Created client-{n}");
    Ok(client)
}

async fn start_picomintd(base: &Path, peer_idx: usize) -> anyhow::Result<Child> {
    let p2p_port = GUARDIAN_BASE_PORT + (peer_idx as u16 * PORTS_PER_GUARDIAN);
    let ui_port = p2p_port + 1;

    let data_dir = base.join(format!("picomintd-{peer_idx}"));
    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join(format!("picomintd-{peer_idx}.log")))?;

    let child = Command::new("target/debug/picomint-server-daemon")
        .env("IN_TEST_ENV", "1")
        .env("DATA_DIR", data_dir.to_str().unwrap())
        .env("BITCOIN_NETWORK", "regtest")
        .env("BITCOIND_URL", format!("http://127.0.0.1:{BTC_RPC_PORT}"))
        .env("BITCOIND_USERNAME", BTC_RPC_USER)
        .env("BITCOIND_PASSWORD", BTC_RPC_PASS)
        .env("P2P_ADDR", format!("127.0.0.1:{p2p_port}"))
        .env("UI_ADDR", format!("127.0.0.1:{ui_port}"))
        .env("UI_PASSWORD", "test")
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start picomintd-{peer_idx}"))?;

    info!("Started picomintd-{peer_idx} on port {p2p_port} (UI: http://127.0.0.1:{ui_port})");
    Ok(child)
}

async fn start_gatewayd(base: &Path, name: &str, gw_port: u16, ln_port: u16) -> anyhow::Result<()> {
    let data_dir = base.join(name);

    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::File::create(base.join(format!("{name}.log")))?;

    Command::new("target/debug/picomint-gateway-daemon")
        .env("IN_TEST_ENV", "1")
        .env("DATA_DIR", data_dir.to_str().unwrap())
        .env("API_ADDR", format!("0.0.0.0:{gw_port}"))
        .env("LDK_ADDR", format!("0.0.0.0:{ln_port}"))
        .env("BITCOIN_NETWORK", "regtest")
        .env("BITCOIND_URL", format!("http://127.0.0.1:{BTC_RPC_PORT}"))
        .env("BITCOIND_USERNAME", BTC_RPC_USER)
        .env("BITCOIND_PASSWORD", BTC_RPC_PASS)
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start {name}"))?;

    info!("Started {name} on port {gw_port}");
    Ok(())
}

async fn run_dkg(peer_data_dirs: &[std::path::PathBuf]) -> anyhow::Result<()> {
    use picomint_server_cli_core::SetupStatus;

    // Wait for all guardians to be ready (the CLI `setup status` call
    // returns once the daemon has bound its CLI socket).
    for (peer, data_dir) in peer_data_dirs.iter().enumerate() {
        retry(&format!("picomintd-{peer} setup status"), || async {
            let status = cli::picomintd_setup_status(data_dir)?;
            ensure!(
                status == SetupStatus::AwaitingLocalParams,
                "Unexpected status: {status:?}"
            );
            Ok(())
        })
        .await?;
    }
    info!("All guardians awaiting local params");

    // Set local params: peer 0 is leader, rest are followers
    let mut setup_codes = BTreeMap::new();
    for (peer, data_dir) in peer_data_dirs.iter().enumerate() {
        let name = format!("Guardian {peer}");
        let (federation_name, federation_size) = if peer == 0 {
            (Some("Test Federation"), Some(NUM_GUARDIANS as u32))
        } else {
            (None, None)
        };
        let resp = cli::picomintd_setup_set_local_params(
            data_dir,
            &name,
            federation_name,
            federation_size,
        )?;
        let setup_code = resp
            .get("setup_code")
            .and_then(|v| v.as_str())
            .context("missing setup_code in set-local-params response")?
            .to_string();
        setup_codes.insert(peer, setup_code);
    }
    info!("Local params set for all guardians");

    // Exchange peer connection info
    for (peer, code) in &setup_codes {
        for (other_peer, data_dir) in peer_data_dirs.iter().enumerate() {
            if other_peer == *peer {
                continue;
            }
            cli::picomintd_setup_add_peer(data_dir, code)?;
        }
    }
    info!("Peer info exchanged");

    // Start DKG on all peers
    for data_dir in peer_data_dirs {
        cli::picomintd_setup_start_dkg(data_dir)?;
    }

    info!("DKG started");
    Ok(())
}

fn build_ldk_node(
    base: &Path,
    runtime: Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<Arc<ldk_node::Node>> {
    let mut builder = ldk_node::Builder::new();

    builder.set_network(Network::Regtest);
    builder.set_node_alias("test-ldk-node".to_string())?;
    builder.set_listening_addresses(vec![
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, TEST_LDK_PORT).into(),
    ])?;
    builder.set_storage_dir_path(
        base.join("test-ldk-node")
            .to_str()
            .context("ldk storage path")?
            .to_string(),
    );
    builder.set_chain_source_bitcoind_rpc(
        "127.0.0.1".to_string(),
        BTC_RPC_PORT,
        BTC_RPC_USER.to_string(),
        BTC_RPC_PASS.to_string(),
    );

    let node = Arc::new(builder.build()?);
    node.start_with_runtime(runtime)?;

    Ok(node)
}

async fn open_channel(
    bitcoind: &bitcoincore_rpc::Client,
    gw_data_dir: &std::path::Path,
    ldk_node: &ldk_node::Node,
) -> anyhow::Result<()> {
    let addr = cli::gatewayd_ldk_onchain_receive(gw_data_dir)?
        .address
        .assume_checked();

    block_in_place(|| bitcoind.generate_to_address(1, &addr))?;
    block_in_place(|| bitcoind.generate_to_address(100, &dummy_address()))?;

    let target_height = block_in_place(|| bitcoind.get_block_count())? - 1;
    retry("gateway sync", || async {
        let info = cli::gatewayd_info(gw_data_dir)?;
        ensure!(
            info.block_height >= target_height,
            "not synced: {} < {target_height}",
            info.block_height,
        );
        Ok(())
    })
    .await?;

    let ldk_pubkey = ldk_node.node_id().to_string();
    let ldk_ln_addr = format!("127.0.0.1:{TEST_LDK_PORT}");

    cli::gatewayd_ldk_channel_open(
        gw_data_dir,
        &ldk_pubkey,
        &ldk_ln_addr,
        10_000_000,
        5_000_000,
    )?;

    // Wait for the funding tx to be negotiated
    let funding_txid = retry("funding tx", || async {
        cli::gatewayd_ldk_channel_list(gw_data_dir)?
            .channels
            .into_iter()
            .find_map(|c| c.funding_txid)
            .context("no funding txid yet")
    })
    .await?;

    // Wait for the funding tx to enter the mempool
    retry("funding tx in mempool", || async {
        block_in_place(|| bitcoind.get_mempool_entry(&funding_txid))
            .map(|_| ())
            .context("funding tx not in mempool")
    })
    .await?;

    // Mine to confirm channel
    block_in_place(|| bitcoind.generate_to_address(10, &dummy_address()))?;

    // Wait for channel to be active on the gateway side
    retry("channel active", || async {
        let channels = cli::gatewayd_ldk_channel_list(gw_data_dir)?.channels;
        ensure!(
            channels.iter().any(|c| c.is_usable),
            "no active channels yet"
        );
        Ok(())
    })
    .await?;

    Ok(())
}

pub async fn retry<F, Fut, T>(name: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    for i in 0..240 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if i == 239 {
                    return Err(e).context(format!("retry '{name}' exhausted after 240 attempts"));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    unreachable!()
}
