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
use picomint_walletv2_client::WalletClientModule;
use tokio::process::Command;
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

pub const PASSWORD: &str = "theresnosecondbest";
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
    pub gw_addr: String,
    pub gw_public: String,
    pub endpoint: Endpoint,
    pub client_counter: AtomicU64,
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

        for i in 0..NUM_GUARDIANS {
            runtime.block_on(start_picomintd(base, i))?;
        }

        info!("Running DKG...");
        runtime.block_on(run_dkg())?;

        let peer0_cli = format!("http://127.0.0.1:{}", GUARDIAN_BASE_PORT + 4);
        let invite_code_str = runtime.block_on(retry("fetch invite code", || async {
            let http = reqwest::Client::new();
            let resp: picomint_server_cli_core::InviteResponse = http
                .post(format!(
                    "{peer0_cli}{}",
                    picomint_server_cli_core::ROUTE_INVITE
                ))
                .json(&())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(resp.invite_code)
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

        // Admin API is on port+1, bound to localhost
        let gw_addr = format!("http://127.0.0.1:{}", GW_PORT + 1);
        // Public API is on the base port (for LNv2 protocol)
        let gw_public = format!("http://127.0.0.1:{GW_PORT}");

        info!("Waiting for gateway...");
        runtime.block_on(retry("gw ready", || async {
            cli::gatewayd_info(&gw_addr).map(|_| ())
        }))?;
        info!("Gateway ready");

        info!("Connecting gateway to federation...");
        cli::gatewayd_federation_join(&gw_addr, invite_code_str.trim())?;
        info!("Gateway connected");

        info!("Building freestanding LDK node...");
        let ldk_node = build_ldk_node(base, runtime.clone())?;
        info!("LDK node built: {}", ldk_node.node_id());

        info!("Funding gateway and opening channel to LDK node...");
        runtime.block_on(open_channel(&bitcoind, &gw_addr, &ldk_node))?;
        info!("Channel opened");

        Ok((
            Self {
                ldk_node,
                data_dir,
                bitcoind,
                invite_code,
                gw_addr,
                gw_public,
                endpoint,
                client_counter,
            },
            client_send,
        ))
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
        let walletv2 = client.get_first_module::<WalletClientModule>()?;
        let addr = walletv2.receive().await;
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

async fn start_picomintd(base: &Path, peer_idx: usize) -> anyhow::Result<()> {
    let port_base = GUARDIAN_BASE_PORT + (peer_idx as u16 * PORTS_PER_GUARDIAN);
    let p2p_port = port_base;
    let ui_port = port_base + 2;
    let cli_port = port_base + 4;

    let data_dir = base.join(format!("picomintd-{peer_idx}"));
    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::File::create(base.join(format!("picomintd-{peer_idx}.log")))?;

    Command::new("target/debug/picomint-server-daemon")
        .env("IN_TEST_ENV", "1")
        .env("DATA_DIR", data_dir.to_str().unwrap())
        .env("BITCOIN_NETWORK", "regtest")
        .env("BITCOIND_URL", format!("http://127.0.0.1:{BTC_RPC_PORT}"))
        .env("BITCOIND_USERNAME", BTC_RPC_USER)
        .env("BITCOIND_PASSWORD", BTC_RPC_PASS)
        .env("BIND_P2P", format!("127.0.0.1:{p2p_port}"))
        .env("BIND_UI", format!("127.0.0.1:{ui_port}"))
        .env("BIND_CLI", format!("127.0.0.1:{cli_port}"))
        .env("UI_PASSWORD", PASSWORD)
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start picomintd-{peer_idx}"))?;

    info!("Started picomintd-{peer_idx} on ports {p2p_port}-{cli_port}");
    Ok(())
}

async fn start_gatewayd(base: &Path, name: &str, gw_port: u16, ln_port: u16) -> anyhow::Result<()> {
    let data_dir = base.join(name);

    tokio::fs::create_dir_all(&data_dir).await?;

    let log_file = std::fs::File::create(base.join(format!("{name}.log")))?;

    Command::new("target/debug/picomint-gateway-daemon")
        .env("IN_TEST_ENV", "1")
        .env("DATA_DIR", data_dir.to_str().unwrap())
        .env("API_BIND", format!("0.0.0.0:{gw_port}"))
        .env("CLI_BIND", format!("127.0.0.1:{}", gw_port + 1))
        .env("LDK_BIND", format!("0.0.0.0:{ln_port}"))
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

async fn run_dkg() -> anyhow::Result<()> {
    use picomint_server_cli_core::{
        ROUTE_SETUP_ADD_PEER, ROUTE_SETUP_SET_LOCAL_PARAMS, ROUTE_SETUP_START_DKG,
        ROUTE_SETUP_STATUS, SetupAddPeerRequest, SetupSetLocalParamsRequest,
        SetupSetLocalParamsResponse, SetupStatus,
    };

    let mut cli_addrs = BTreeMap::new();
    for i in 0..NUM_GUARDIANS {
        let cli_port = GUARDIAN_BASE_PORT + (i as u16 * PORTS_PER_GUARDIAN) + 4;
        cli_addrs.insert(i, format!("http://127.0.0.1:{cli_port}"));
    }

    let http = reqwest::Client::new();

    // Wait for all guardians to be ready
    for (peer, addr) in &cli_addrs {
        retry(&format!("picomintd-{peer} setup status"), || {
            let http = http.clone();
            let addr = addr.clone();
            async move {
                let status: SetupStatus = http
                    .post(format!("{addr}{ROUTE_SETUP_STATUS}"))
                    .json(&())
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                ensure!(
                    status == SetupStatus::AwaitingLocalParams,
                    "Unexpected status: {status:?}"
                );
                Ok(())
            }
        })
        .await?;
    }
    info!("All guardians awaiting local params");

    // Set local params: peer 0 is leader, rest are followers
    let mut setup_codes = BTreeMap::new();
    for (peer, addr) in &cli_addrs {
        let request = SetupSetLocalParamsRequest {
            name: format!("Guardian {peer}"),
            federation_name: if *peer == 0 {
                Some("Test Federation".to_string())
            } else {
                None
            },
            federation_size: if *peer == 0 {
                Some(NUM_GUARDIANS as u32)
            } else {
                None
            },
        };

        let resp: SetupSetLocalParamsResponse = http
            .post(format!("{addr}{ROUTE_SETUP_SET_LOCAL_PARAMS}"))
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        setup_codes.insert(*peer, resp.setup_code);
    }
    info!("Local params set for all guardians");

    // Exchange peer connection info
    for (peer, code) in &setup_codes {
        for (other_peer, addr) in &cli_addrs {
            if other_peer == peer {
                continue;
            }
            http.post(format!("{addr}{ROUTE_SETUP_ADD_PEER}"))
                .json(&SetupAddPeerRequest {
                    setup_code: code.clone(),
                })
                .send()
                .await?
                .error_for_status()?;
        }
    }
    info!("Peer info exchanged");

    // Start DKG on all peers
    for addr in cli_addrs.values() {
        http.post(format!("{addr}{ROUTE_SETUP_START_DKG}"))
            .json(&())
            .send()
            .await?
            .error_for_status()?;
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
    gw_addr: &str,
    ldk_node: &ldk_node::Node,
) -> anyhow::Result<()> {
    let addr = cli::gatewayd_ldk_onchain_receive(gw_addr)?
        .address
        .assume_checked();

    block_in_place(|| bitcoind.generate_to_address(1, &addr))?;
    block_in_place(|| bitcoind.generate_to_address(100, &dummy_address()))?;

    let target_height = block_in_place(|| bitcoind.get_block_count())? - 1;
    retry("gateway sync", || async {
        let info = cli::gatewayd_info(gw_addr)?;
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

    cli::gatewayd_ldk_channel_open(gw_addr, &ldk_pubkey, &ldk_ln_addr, 10_000_000, 5_000_000)?;

    // Wait for the funding tx to be negotiated
    let funding_txid = retry("funding tx", || async {
        cli::gatewayd_ldk_channel_list(gw_addr)?
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
        let channels = cli::gatewayd_ldk_channel_list(gw_addr)?.channels;
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
