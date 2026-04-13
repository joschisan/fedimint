use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use bitcoincore_rpc::RpcApi;
use fedimint_api_client::connection::ConnectionPool;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_core::Amount;
use fedimint_core::invite_code::InviteCode;
use fedimint_gateway_cli_core::{
    InfoResponse, LdkBalancesResponse, LdkChannelListResponse, LdkOnchainReceiveResponse,
    WalletReceiveResponse,
};
use fedimint_walletv2_client::WalletClientModule;
use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use tokio::process::Command;
use tokio::task::block_in_place;
use tracing::info;

use crate::cli::{RunGatewayCli, gateway_cmd};

pub fn find_binary(name: &str) -> PathBuf {
    let target_dir =
        std::env::var("CARGO_BUILD_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let profile = std::env::var("CARGO_PROFILE_DIR").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(format!("{target_dir}/{profile}/{name}"))
}

pub const BTC_RPC_PORT: u16 = 18443;
pub const GUARDIAN_BASE_PORT: u16 = 17000;
pub const PORTS_PER_GUARDIAN: u16 = 5;
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

fn dummy_address() -> bitcoin::Address {
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

        // Fund bitcoind's own wallet so peg-ins can be regular (non-coinbase)
        // transactions — avoids the 100-block coinbase maturity wait.
        let funding_addr = block_in_place(|| bitcoind.get_new_address(None, None))?
            .require_network(bitcoin::Network::Regtest)?;
        block_in_place(|| bitcoind.generate_to_address(101, &funding_addr))?;

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
        retry("gw1 ready", || async {
            gateway_cmd(&gw1_addr)
                .arg("info")
                .run_gateway_cli::<InfoResponse>()
                .map(|_| ())
        })
        .await?;
        retry("gw2 ready", || async {
            gateway_cmd(&gw2_addr)
                .arg("info")
                .run_gateway_cli::<InfoResponse>()
                .map(|_| ())
        })
        .await?;
        info!("Gateways ready");

        info!("Connecting gateways to federation...");
        gateway_cmd(&gw1_addr)
            .arg("federation")
            .arg("join")
            .arg(invite_code_str.trim())
            .run_gateway_cli::<serde_json::Value>()?;
        gateway_cmd(&gw2_addr)
            .arg("federation")
            .arg("join")
            .arg(invite_code_str.trim())
            .run_gateway_cli::<serde_json::Value>()?;
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
            block_in_place(|| client.get_blockchain_info()).context("bitcoind not reachable")
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

        let endpoint = Endpoint::builder(N0).bind().await?;
        let connectors = ConnectionPool::new(endpoint);

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

    pub async fn pegin_gateway(
        &self,
        gw_addr: &str,
        amount: bitcoin::Amount,
    ) -> anyhow::Result<()> {
        let fed_id = self.invite_code.federation_id().to_string();

        let addr = gateway_cmd(gw_addr)
            .args(["module", &fed_id, "walletv2", "receive"])
            .run_gateway_cli::<WalletReceiveResponse>()?
            .address
            .assume_checked();

        self.send_to_address(&addr, amount)?;
        self.mine_blocks(10);

        retry("gateway pegin balance", || {
            let gw_addr = gw_addr.to_string();
            let fed_id = fed_id.clone();
            async move {
                let balances = gateway_cmd(&gw_addr)
                    .arg("ldk")
                    .arg("balances")
                    .run_gateway_cli::<LdkBalancesResponse>()?;

                let fed_balance = balances
                    .ecash_balances
                    .iter()
                    .find(|b| b.federation_id.to_string() == fed_id)
                    .context("federation not found")?;

                ensure!(
                    fed_balance.ecash_balance_msats.msats > 0,
                    "gateway balance is zero"
                );
                Ok(())
            }
        })
        .await?;

        info!("Pegged in gateway {gw_addr}");
        Ok(())
    }

    pub async fn pegin(
        &self,
        client: &ClientHandleArc,
        amount: bitcoin::Amount,
    ) -> anyhow::Result<()> {
        let walletv2 = client.get_first_module::<WalletClientModule>()?;
        let addr = walletv2.receive().await;

        self.send_to_address(&addr, amount)?;
        self.mine_blocks(10);

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
    let cli_port = port_base + 4;

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
        .env("FM_BIND_CLI", format!("127.0.0.1:{cli_port}"))
        .env("FM_UI_PASSWORD", PASSWORD)
        .env("FM_P2P_URL", format!("fedimint://127.0.0.1:{p2p_port}"))
        .env("FM_API_URL", format!("ws://127.0.0.1:{api_port}"))
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .context(format!("Failed to start fedimintd-{peer_idx}"))?;

    info!("Started fedimintd-{peer_idx} on ports {p2p_port}-{cli_port}");
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
        .env("FM_GATEWAY_API_BIND", format!("0.0.0.0:{gw_port}"))
        .env("FM_GATEWAY_CLI_BIND", format!("127.0.0.1:{}", gw_port + 1))
        .env("FM_LDK_BIND", format!("0.0.0.0:{ln_port}"))
        .env(
            "FM_GATEWAY_METRICS_LISTEN_ADDR",
            format!("127.0.0.1:{metrics_port}"),
        )
        .env("FM_GATEWAY_SKIP_WAIT_FOR_SYNC", "1")
        .env("FM_GATEWAY_NETWORK", "regtest")
        .env("FM_DEFAULT_ROUTING_FEES", "0,0")
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
    use fedimint_server_cli_core::{
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
        retry(&format!("fedimintd-{peer} setup status"), || {
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

async fn open_channel_between_gateways(
    bitcoind: &bitcoincore_rpc::Client,
    gw1_addr: &str,
    gw2_addr: &str,
) -> anyhow::Result<()> {
    let gw2_info = gateway_cmd(gw2_addr)
        .arg("info")
        .run_gateway_cli::<InfoResponse>()?;

    for gw_addr in [gw1_addr, gw2_addr] {
        let addr = gateway_cmd(gw_addr)
            .args(["ldk", "onchain", "receive"])
            .run_gateway_cli::<LdkOnchainReceiveResponse>()?
            .address;

        let addr: bitcoin::Address<bitcoin::address::NetworkUnchecked> = addr.parse()?;
        let addr = addr.assume_checked();
        block_in_place(|| bitcoind.generate_to_address(1, &addr))?;
        block_in_place(|| bitcoind.generate_to_address(100, &dummy_address()))?;
    }

    let target_height = block_in_place(|| bitcoind.get_block_count())? - 1;
    for gw_addr in [gw1_addr, gw2_addr] {
        retry("gateway sync", || {
            let gw_addr = gw_addr.to_string();
            async move {
                let info = gateway_cmd(&gw_addr)
                    .arg("info")
                    .run_gateway_cli::<InfoResponse>()?;
                ensure!(
                    info.block_height >= target_height,
                    "not synced: {} < {target_height}",
                    info.block_height,
                );
                Ok(())
            }
        })
        .await?;
    }

    let gw2_pubkey = gw2_info.public_key.to_string();

    let gw2_ln_addr = format!("127.0.0.1:{GW2_LN_PORT}");

    gateway_cmd(gw1_addr)
        .args([
            "ldk",
            "channel",
            "open",
            &gw2_pubkey,
            &gw2_ln_addr,
            "10000000",
            "--push-amount-sats",
            "5000000",
        ])
        .run_gateway_cli::<serde_json::Value>()?;

    // Wait for the funding tx to be negotiated
    retry("funding tx", || {
        let gw1_addr = gw1_addr.to_string();
        async move {
            let channels = gateway_cmd(&gw1_addr)
                .args(["ldk", "channel", "list"])
                .run_gateway_cli::<LdkChannelListResponse>()?
                .channels;
            ensure!(!channels.is_empty(), "no channels yet");
            Ok(())
        }
    })
    .await?;

    // Mine to confirm channel
    block_in_place(|| bitcoind.generate_to_address(10, &dummy_address()))?;

    // Wait for channel to be active on both sides
    for gw_addr in [gw1_addr, gw2_addr] {
        retry("channel active", || {
            let gw_addr = gw_addr.to_string();
            async move {
                let channels = gateway_cmd(&gw_addr)
                    .args(["ldk", "channel", "list"])
                    .run_gateway_cli::<LdkChannelListResponse>()?
                    .channels;
                ensure!(
                    channels.iter().any(|c| c.is_usable),
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
