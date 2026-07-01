use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::{FeeRate, Network, OutPoint};
use fedimint_bip39::Mnemonic;
use fedimint_core::envs::{FM_IN_DEVIMINT_ENV, is_env_var_set, is_running_in_test_env};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::task::block_in_place;
use fedimint_core::util::{FmtCompact, SafeUrl, backoff_util, retry};
use fedimint_core::{Amount, BitcoinAmountOrAll, crit};
use fedimint_gateway_common::{
    ChainSource, ChannelInfo, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse,
    OpenChannelRequest, SendOnchainRequest,
};
use fedimint_lightning::{InterceptPaymentResponse, LightningRpcError, PaymentAction, Preimage};
use fedimint_logging::{LOG_LIGHTNING, LOG_LIGHTNING_LDK};
use ldk_node::config::ChannelConfig;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::{NodeAlias, NodeId};
use ldk_node::logger::{LogLevel, LogRecord, LogWriter};
use ldk_node::payment::{PaymentKind, PaymentStatus, SendingParameters};
use lightning::ln::channelmanager::PaymentId;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use super::{GetBalancesResponse, GetNodeInfoResponse};
use crate::Gateway;

/// Forwards `ldk-node`'s log records into the gateway's `tracing` subscriber.
///
/// By default `ldk-node` writes to its own append-only `ldk_node/ldk_node.log`
/// file, which is invisible to stdout/stderr log collectors and grows without
/// bound. Routing the records through `tracing` (under the
/// [`LOG_LIGHTNING_LDK`] target) puts them alongside the rest of gatewayd's
/// logs and makes them filterable via `RUST_LOG`.
struct LdkTracingLogger {
    /// Whether we're running under devimint/tests. When set, some benign LDK
    /// error logs that are expected in regtest are downgraded to avoid spamming
    /// the test output. See [`Self::downgraded_level`].
    in_test_env: bool,
}

impl LdkTracingLogger {
    /// Returns the level to emit `record` at, downgrading benign-but-noisy LDK
    /// errors when running under devimint/tests.
    ///
    /// In regtest there is no fee-rate history, so `ldk-node` logs "Failed to
    /// retrieve fee rate estimates ... Falling back to default" at `Error` on
    /// essentially every sync. This is harmless (LDK falls back to a default
    /// feerate), so in test environments we emit it at `Debug` instead. In
    /// production the original `Error` level is preserved, since a persistent
    /// failure there can indicate a real problem.
    fn downgraded_level(&self, record: &LogRecord<'_>) -> LogLevel {
        if self.in_test_env
            && record.level == LogLevel::Error
            && record.module_path == "ldk_node::chain"
            && format!("{}", record.args).contains("Failed to retrieve fee rate estimates")
        {
            LogLevel::Debug
        } else {
            record.level
        }
    }
}

impl LogWriter for LdkTracingLogger {
    fn log(&self, record: LogRecord<'_>) {
        // `tracing` requires a static level per call-site, so match each LDK level.
        match self.downgraded_level(&record) {
            LogLevel::Gossip | LogLevel::Trace => tracing::trace!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Debug => debug!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Info => info!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Warn => warn!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Error => error!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
        }
    }
}

/// Builds and starts the LDK node from the gateway's configuration and returns
/// it. The node is stopped on gateway shutdown (see [`Gateway::run`]); the
/// gateway's lightning bookkeeping (the pay lock-pool and pending-channels map)
/// lives on [`Gateway`] itself.
pub(crate) fn build_ldk_node(
    data_dir: &Path,
    chain_source: &ChainSource,
    network: Network,
    ldk_addr: SocketAddr,
    alias: String,
    mnemonic: Mnemonic,
    runtime: Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<Arc<ldk_node::Node>> {
    let mut bytes = [0u8; 32];
    let alias = if alias.is_empty() {
        "LDK Gateway".to_string()
    } else {
        alias
    };
    let alias_bytes = alias.as_bytes();
    let truncated = &alias_bytes[..alias_bytes.len().min(32)];
    bytes[..truncated.len()].copy_from_slice(truncated);
    let node_alias = Some(NodeAlias(bytes));

    let listening_address = match ldk_addr {
        SocketAddr::V4(addr) => SocketAddress::TcpIpV4 {
            addr: addr.ip().octets(),
            port: addr.port(),
        },
        SocketAddr::V6(addr) => SocketAddress::TcpIpV6 {
            addr: addr.ip().octets(),
            port: addr.port(),
        },
    };

    let mut node_builder = ldk_node::Builder::from_config(ldk_node::config::Config {
        network,
        listening_addresses: Some(vec![listening_address]),
        node_alias,
        ..Default::default()
    });

    // Route LDK's logs into the gateway's `tracing` subscriber so they land in
    // the same place (stderr / log file) and honor `RUST_LOG`, instead of LDK's
    // default append-only `ldk_node/ldk_node.log` file.
    node_builder.set_custom_logger(Arc::new(LdkTracingLogger {
        in_test_env: is_running_in_test_env(),
    }));

    node_builder.set_entropy_bip39_mnemonic(mnemonic, None);

    match chain_source.clone() {
        ChainSource::Bitcoind {
            username,
            password,
            server_url,
        } => {
            node_builder.set_chain_source_bitcoind_rpc(
                server_url
                    .host_str()
                    .expect("Could not retrieve host from bitcoind RPC url")
                    .to_string(),
                server_url
                    .port()
                    .expect("Could not retrieve port from bitcoind RPC url"),
                username,
                password,
            );
        }
        ChainSource::Esplora { server_url } => {
            node_builder.set_chain_source_esplora(get_esplora_url(&server_url)?, None);
        }
    }
    let Some(data_dir_str) = data_dir.to_str() else {
        return Err(anyhow::anyhow!("Invalid data dir path"));
    };
    node_builder.set_storage_dir_path(data_dir_str.to_string());

    info!(chain_source = %chain_source, data_dir = %data_dir_str, alias = %alias, "Starting LDK Node...");
    let node = Arc::new(node_builder.build()?);
    node.start_with_runtime(runtime).map_err(|err| {
        crit!(target: LOG_LIGHTNING, err = %err.fmt_compact(), "Failed to start LDK Node");
        LightningRpcError::FailedToConnect
    })?;

    info!("Successfully started LDK Gateway");
    Ok(node)
}

impl Gateway {
    /// Drives the LDK event queue until an inbound payment becomes claimable,
    /// returning its payment hash and claimable amount. Channel lifecycle
    /// events (`ChannelPending` / `ChannelClosed`) are handled inline to
    /// unblock [`Self::open_channel`]; all other events are ignored. Each
    /// event is acknowledged with `event_handled` before the next is
    /// pulled.
    ///
    /// The gateway calls this in a loop (see `Gateway::process_ldk_events`); on
    /// shutdown the caller cancels the future at the `next_event_async` await.
    pub async fn next_incoming_payment(&self) -> (sha256::Hash, u64) {
        loop {
            let event = self.node().next_event_async().await;

            let claimable = match event {
                ldk_node::Event::PaymentClaimable {
                    payment_hash,
                    claimable_amount_msat,
                    ..
                } => Some((
                    sha256::Hash::from_slice(&payment_hash.0).expect("Failed to create Hash"),
                    claimable_amount_msat,
                )),
                ldk_node::Event::ChannelPending {
                    channel_id,
                    user_channel_id,
                    funding_txo,
                    ..
                } => {
                    info!(target: LOG_LIGHTNING, %channel_id, "LDK Channel is pending");
                    if let Some(sender) = self
                        .pending_channels
                        .write()
                        .await
                        .remove(&UserChannelId(user_channel_id))
                    {
                        let _ = sender.send(Ok(funding_txo));
                    } else {
                        debug!(
                            ?user_channel_id,
                            "No channel open pending for user channel id"
                        );
                    }
                    None
                }
                ldk_node::Event::ChannelClosed {
                    channel_id,
                    user_channel_id,
                    reason,
                    ..
                } => {
                    info!(target: LOG_LIGHTNING, %channel_id, "LDK Channel is closed");
                    if let Some(sender) = self
                        .pending_channels
                        .write()
                        .await
                        .remove(&UserChannelId(user_channel_id))
                    {
                        let reason = reason.map_or_else(
                            || "Channel has been closed".to_string(),
                            |r| r.to_string(),
                        );
                        let _ = sender.send(Err(anyhow::anyhow!(reason)));
                    } else {
                        debug!(
                            ?user_channel_id,
                            "No channel open pending for user channel id"
                        );
                    }
                    None
                }
                _ => None,
            };

            if let Err(err) = self.node().event_handled() {
                warn!(err = %err.fmt_compact(), "LDK could not mark event handled");
            }

            if let Some(payment) = claimable {
                return payment;
            }
        }
    }
}

impl Gateway {
    /// The lightning node's public key (its node id). Cheap, local read.
    pub fn public_key(&self) -> PublicKey {
        self.node().node_id()
    }

    /// The lightning node's alias, or a default derived from its node id.
    /// Cheap, local read.
    pub fn alias(&self) -> String {
        self.node().node_alias().map_or_else(
            || format!("LDK Fedimint Gateway Node {}", self.node().node_id()),
            |alias| alias.to_string(),
        )
    }

    /// Returns high-level info about the lightning node.
    pub fn info(&self) -> GetNodeInfoResponse {
        let node_status = self.node().status();
        let ldk_block_height = node_status.current_best_block.height;
        let onchain_sync = node_status.latest_onchain_wallet_sync_timestamp;
        let lightning_sync = node_status.latest_lightning_wallet_sync_timestamp;
        let is_running = node_status.is_running;
        debug!(target: LOG_LIGHTNING, ?onchain_sync, ?lightning_sync, ?is_running, "LDK Sync Status");

        GetNodeInfoResponse {
            pub_key: self.node().node_id(),
            alias: match self.node().node_alias() {
                Some(alias) => alias.to_string(),
                None => format!("LDK Fedimint Gateway Node {}", self.node().node_id()),
            },
            network: self.node().config().network.to_string(),
            block_height: ldk_block_height,
            // `synced_to_chain` is used for determining if the Lightning node is ready, so we care
            // about the `lightning_sync` status.
            synced_to_chain: lightning_sync.is_some(),
        }
    }

    /// Attempts to pay an invoice using the lightning node, waiting for the
    /// payment to complete and returning the preimage.
    ///
    /// This is idempotent for a given invoice: if a payment is already in
    /// flight it waits for that one to complete instead of starting another.
    pub async fn pay(
        &self,
        invoice: &Bolt11Invoice,
        max_delay: u64,
        max_fee: Amount,
    ) -> Result<Preimage, LightningRpcError> {
        let payment_id = PaymentId(*invoice.payment_hash().as_byte_array());

        // Lock by the payment hash to prevent multiple simultaneous calls with the same
        // invoice from executing. This prevents `ldk-node::Bolt11Payment::send()` from
        // being called multiple times with the same invoice. This is important because
        // `ldk-node::Bolt11Payment::send()` is not idempotent, but this function must
        // be idempotent.
        let _payment_lock_guard = self
            .outbound_lightning_payment_lock_pool
            .async_lock(payment_id)
            .await;

        // If a payment is not known to the node we can initiate it, and if it is known
        // we can skip calling `ldk-node::Bolt11Payment::send()` and wait for the
        // payment to complete. The lock guard above guarantees that this block is only
        // executed once at a time for a given payment hash, ensuring that there is no
        // race condition between checking if a payment is known and initiating a new
        // payment if it isn't.
        if self.node().payment(&payment_id).is_none() {
            assert_eq!(
                self.node()
                    .bolt11_payment()
                    .send(
                        invoice,
                        Some(SendingParameters {
                            max_total_routing_fee_msat: Some(Some(max_fee.msats)),
                            max_total_cltv_expiry_delta: Some(max_delay as u32),
                            max_path_count: None,
                            max_channel_saturation_power_of_half: None,
                        }),
                    )
                    // TODO: Investigate whether all error types returned by `Bolt11Payment::send()`
                    // result in idempotency.
                    .map_err(|e| LightningRpcError::FailedPayment {
                        failure_reason: format!("LDK payment failed to initialize: {e:?}"),
                    })?,
                payment_id
            );
        }

        // TODO: Find a way to avoid looping/polling to know when a payment is
        // completed. `ldk-node` provides `PaymentSuccessful` and `PaymentFailed`
        // events, but interacting with the node event queue here isn't
        // straightforward.
        loop {
            if let Some(payment_details) = self.node().payment(&payment_id) {
                match payment_details.status {
                    PaymentStatus::Pending => {}
                    PaymentStatus::Succeeded => {
                        if let PaymentKind::Bolt11 {
                            preimage: Some(preimage),
                            ..
                        } = payment_details.kind
                        {
                            return Ok(Preimage(preimage.0));
                        }
                    }
                    PaymentStatus::Failed => {
                        return Err(LightningRpcError::FailedPayment {
                            failure_reason: "LDK payment failed".to_string(),
                        });
                    }
                }
            }
            fedimint_core::runtime::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Settles or fails a claimable inbound payment on the lightning node,
    /// per the [`PaymentAction`] in the response.
    pub fn complete_htlc_once(
        &self,
        htlc: InterceptPaymentResponse,
    ) -> Result<(), LightningRpcError> {
        let InterceptPaymentResponse {
            action,
            payment_hash,
            incoming_chan_id: _,
            htlc_id: _,
        } = htlc;

        let ph = PaymentHash(*payment_hash.clone().as_byte_array());

        // TODO: Get the actual amount from the LDK node. Probably makes the
        // most sense to pipe it through the `InterceptHtlcResponse` struct.
        // This value is only used by `ldk-node` to ensure that the amount
        // claimed isn't less than the amount expected, but we've already
        // verified that the amount is correct when we intercepted the payment.
        let claimable_amount_msat = 999_999_999_999_999;

        let ph_hex_str = hex::encode(payment_hash);

        if let PaymentAction::Settle(preimage) = action {
            self.node()
                .bolt11_payment()
                .claim_for_hash(ph, claimable_amount_msat, PaymentPreimage(preimage.0))
                .map_err(|_| LightningRpcError::FailedToCompleteHtlc {
                    failure_reason: format!("Failed to claim LDK payment with hash {ph_hex_str}"),
                })?;
        } else {
            warn!(target: LOG_LIGHTNING, payment_hash = %ph_hex_str, "Unwinding payment because the action was not `Settle`");
            self.node()
                .bolt11_payment()
                .fail_for_hash(ph)
                .map_err(|_| LightningRpcError::FailedToCompleteHtlc {
                    failure_reason: format!("Failed to unwind LDK payment with hash {ph_hex_str}"),
                })?;
        }

        Ok(())
    }

    /// Requests the lightning node to create an invoice. A `payment_hash` makes
    /// the invoice an LNv2 receive (`receive_for_hash`, matched to an incoming
    /// contract); its absence makes it a plain invoice payable directly to this
    /// node.
    pub fn create_invoice(
        &self,
        payment_hash: Option<sha256::Hash>,
        amount_msat: u64,
        description: &fedimint_lnv2_common::Bolt11InvoiceDescription,
        expiry_secs: u32,
    ) -> Result<Bolt11Invoice, LightningRpcError> {
        let description = match description {
            fedimint_lnv2_common::Bolt11InvoiceDescription::Direct(desc) => {
                Bolt11InvoiceDescription::Direct(Description::new(desc.clone()).map_err(|_| {
                    LightningRpcError::FailedToGetInvoice {
                        failure_reason: "Invalid description".to_string(),
                    }
                })?)
            }
            fedimint_lnv2_common::Bolt11InvoiceDescription::Hash(hash) => {
                Bolt11InvoiceDescription::Hash(lightning_invoice::Sha256(*hash))
            }
        };

        let invoice = match payment_hash {
            Some(payment_hash) => self.node().bolt11_payment().receive_for_hash(
                amount_msat,
                &description,
                expiry_secs,
                PaymentHash(*payment_hash.as_byte_array()),
            ),
            None => self
                .node()
                .bolt11_payment()
                .receive(amount_msat, &description, expiry_secs),
        }
        .map_err(|e| LightningRpcError::FailedToGetInvoice {
            failure_reason: e.to_string(),
        })?;

        Bolt11Invoice::from_str(&invoice.to_string()).map_err(|e| {
            LightningRpcError::FailedToGetInvoice {
                failure_reason: e.to_string(),
            }
        })
    }

    /// Gets a funding address belonging to the lightning node's on-chain
    /// wallet.
    pub fn get_ln_onchain_address(&self) -> Result<String, LightningRpcError> {
        self.node()
            .onchain_payment()
            .new_address()
            .map(|address| address.to_string())
            .map_err(|e| LightningRpcError::FailedToGetLnOnchainAddress {
                failure_reason: e.to_string(),
            })
    }

    /// Executes an onchain transaction using the lightning node's on-chain
    /// wallet.
    pub fn send_onchain(
        &self,
        SendOnchainRequest {
            address,
            amount,
            fee_rate_sats_per_vbyte,
        }: SendOnchainRequest,
    ) -> Result<String, LightningRpcError> {
        let onchain = self.node().onchain_payment();

        let retain_reserves = false;
        let txid = match amount {
            BitcoinAmountOrAll::All => onchain.send_all_to_address(
                &address.assume_checked(),
                retain_reserves,
                FeeRate::from_sat_per_vb(fee_rate_sats_per_vbyte),
            ),
            BitcoinAmountOrAll::Amount(amount_sats) => onchain.send_to_address(
                &address.assume_checked(),
                amount_sats.to_sat(),
                FeeRate::from_sat_per_vb(fee_rate_sats_per_vbyte),
            ),
        }
        .map_err(|e| LightningRpcError::FailedToWithdrawOnchain {
            failure_reason: e.to_string(),
        })?;

        Ok(txid.to_string())
    }

    /// Opens a channel with a peer lightning node.
    pub async fn open_channel(
        &self,
        OpenChannelRequest {
            pubkey,
            host,
            channel_size_sats,
            push_amount_sats,
            fee_rate_sats_per_vbyte,
            base_fee_msat,
            parts_per_million,
        }: OpenChannelRequest,
    ) -> Result<String, LightningRpcError> {
        let push_amount_msats_or = if push_amount_sats == 0 {
            None
        } else {
            Some(push_amount_sats * 1000)
        };

        if fee_rate_sats_per_vbyte.is_some() {
            // LDK manages its own fee estimation for funding transactions; the
            // user-supplied rate cannot be applied here.
            warn!(
                target: LOG_LIGHTNING,
                "Ignoring fee_rate_sats_per_vbyte on LDK channel open; LDK uses its built-in fee estimator"
            );
        }

        let channel_config = match (base_fee_msat, parts_per_million) {
            (None, None) => None,
            (base, ppm) => {
                let mut config = ChannelConfig::default();
                if let Some(base) = base {
                    config.forwarding_fee_base_msat = u32::try_from(base).map_err(|_| {
                        LightningRpcError::FailedToOpenChannel {
                            failure_reason: format!(
                                "base_fee_msat {base} does not fit in u32 (LDK limit)"
                            ),
                        }
                    })?;
                }
                if let Some(ppm) = ppm {
                    config.forwarding_fee_proportional_millionths =
                        u32::try_from(ppm).map_err(|_| LightningRpcError::FailedToOpenChannel {
                            failure_reason: format!(
                                "parts_per_million {ppm} does not fit in u32 (LDK limit)"
                            ),
                        })?;
                }
                Some(config)
            }
        };

        let (tx, rx) = oneshot::channel::<anyhow::Result<OutPoint>>();

        {
            let mut channels = self.pending_channels.write().await;
            let user_channel_id = self
                .node()
                .open_announced_channel(
                    pubkey,
                    SocketAddress::from_str(&host).map_err(|e| {
                        LightningRpcError::FailedToConnectToPeer {
                            failure_reason: e.to_string(),
                        }
                    })?,
                    channel_size_sats,
                    push_amount_msats_or,
                    channel_config,
                )
                .map_err(|e| LightningRpcError::FailedToOpenChannel {
                    failure_reason: e.to_string(),
                })?;

            channels.insert(UserChannelId(user_channel_id), tx);
        }

        match rx
            .await
            .map_err(|err| LightningRpcError::FailedToOpenChannel {
                failure_reason: err.to_string(),
            })? {
            Ok(outpoint) => Ok(outpoint.txid.to_string()),
            Err(err) => Err(LightningRpcError::FailedToOpenChannel {
                failure_reason: err.to_string(),
            }),
        }
    }

    /// Closes all channels with a peer lightning node.
    pub fn close_channels_with_peer(
        &self,
        request: &CloseChannelsWithPeerRequest,
    ) -> CloseChannelsWithPeerResponse {
        let pubkey = request.pubkey;
        let force = request.force;
        let mut num_channels_closed = 0;

        info!(%pubkey, "Closing all channels with peer");
        for channel_with_peer in self
            .node()
            .list_channels()
            .iter()
            .filter(|channel| channel.counterparty_node_id == pubkey)
        {
            if force {
                match self.node().force_close_channel(
                    &channel_with_peer.user_channel_id,
                    pubkey,
                    Some("User initiated force close".to_string()),
                ) {
                    Ok(()) => num_channels_closed += 1,
                    Err(err) => {
                        error!(%pubkey, err = %err.fmt_compact(), "Could not force close channel");
                    }
                }
            } else {
                match self
                    .node()
                    .close_channel(&channel_with_peer.user_channel_id, pubkey)
                {
                    Ok(()) => {
                        num_channels_closed += 1;
                    }
                    Err(err) => {
                        error!(%pubkey, err = %err.fmt_compact(), "Could not close channel");
                    }
                }
            }
        }

        CloseChannelsWithPeerResponse {
            num_channels_closed,
        }
    }

    /// Lists the lightning node's active channels with all peers.
    pub fn list_channels(&self) -> Vec<ChannelInfo> {
        let mut channels = Vec::new();
        let network_graph = self.node().network_graph();

        // Build a map of peer pubkey -> address from connected/known peers
        let peer_addresses: std::collections::HashMap<_, _> = self
            .node()
            .list_peers()
            .into_iter()
            .map(|peer| (peer.node_id, peer.address.to_string()))
            .collect();

        for channel_details in &self.node().list_channels() {
            let node_id = NodeId::from_pubkey(&channel_details.counterparty_node_id);
            let node_info = network_graph.node(&node_id);

            // Look up peer alias from network graph
            let remote_node_alias = node_info.as_ref().and_then(|info| {
                info.announcement_info.as_ref().and_then(|announcement| {
                    let alias = announcement.alias().to_string();
                    if alias.is_empty() { None } else { Some(alias) }
                })
            });

            let remote_address = peer_addresses
                .get(&channel_details.counterparty_node_id)
                .cloned();

            channels.push(ChannelInfo {
                remote_pubkey: channel_details.counterparty_node_id,
                channel_size_sats: channel_details.channel_value_sats,
                outbound_liquidity_sats: channel_details.outbound_capacity_msat / 1000,
                inbound_liquidity_sats: channel_details.inbound_capacity_msat / 1000,
                is_active: channel_details.is_usable,
                funding_outpoint: channel_details.funding_txo,
                remote_node_alias,
                remote_address,
                base_fee_msat: Some(u64::from(channel_details.config.forwarding_fee_base_msat)),
                parts_per_million: Some(u64::from(
                    channel_details
                        .config
                        .forwarding_fee_proportional_millionths,
                )),
            });
        }

        channels
    }

    /// Connects to a lightning peer, persisting the connection so the node
    /// reconnects on restart.
    pub fn connect_peer(&self, node_id: PublicKey, host: &str) -> Result<(), LightningRpcError> {
        let address = SocketAddress::from_str(host).map_err(|e| {
            LightningRpcError::FailedToConnectToPeer {
                failure_reason: e.to_string(),
            }
        })?;
        self.node().connect(node_id, address, true).map_err(|e| {
            LightningRpcError::FailedToConnectToPeer {
                failure_reason: e.to_string(),
            }
        })
    }

    /// Disconnects from a lightning peer.
    pub fn disconnect_peer(&self, node_id: PublicKey) -> Result<(), LightningRpcError> {
        self.node()
            .disconnect(node_id)
            .map_err(|e| LightningRpcError::FailedToConnectToPeer {
                failure_reason: e.to_string(),
            })
    }

    /// Lists the node's lightning peers as `(node_id, address, is_connected)`.
    pub fn list_peers(&self) -> Vec<(PublicKey, String, bool)> {
        self.node()
            .list_peers()
            .into_iter()
            .map(|peer| (peer.node_id, peer.address.to_string(), peer.is_connected))
            .collect()
    }

    /// Returns a summary of the lightning node's balance, including the onchain
    /// wallet, outbound liquidity, and inbound liquidity.
    pub fn get_balances(&self) -> GetBalancesResponse {
        let balances = self.node().list_balances();
        let channel_lists = self
            .node()
            .list_channels()
            .into_iter()
            .filter(|chan| chan.is_usable)
            .collect::<Vec<_>>();
        // map and get the total inbound_capacity_msat in the channels
        let total_inbound_liquidity_balance_msat: u64 = channel_lists
            .iter()
            .map(|channel| channel.inbound_capacity_msat)
            .sum();

        GetBalancesResponse {
            onchain_balance_sats: balances.total_onchain_balance_sats,
            lightning_balance_msats: balances.total_lightning_balance_sats * 1000,
            inbound_lightning_liquidity_msats: total_inbound_liquidity_balance_msat,
        }
    }

    pub fn sync_wallet(&self) {
        block_in_place(|| {
            let _ = self.node().sync_wallets();
        });
    }

    /// Waits for the lightning node to be synced to the Bitcoin blockchain.
    pub async fn wait_for_chain_sync(&self) -> Result<(), LightningRpcError> {
        // In devimint, we explicitly sync the onchain wallet to start the sync quicker
        // than background sync would. In production, background sync is
        // sufficient
        if is_env_var_set(FM_IN_DEVIMINT_ENV) {
            self.sync_wallet();
        }

        // Wait for the Lightning node to sync
        retry(
            "Wait for chain sync",
            backoff_util::background_backoff(),
            || async {
                let info = self.info();
                let block_height = info.block_height;
                if info.synced_to_chain {
                    Ok(())
                } else {
                    warn!(target: LOG_LIGHTNING, block_height = %block_height, "Lightning node is not synced yet");
                    Err(anyhow::anyhow!("Not synced yet"))
                }
            },
        )
        .await
        .map_err(|e| LightningRpcError::FailedToSyncToChain {
            failure_reason: format!("Failed to sync to chain: {e:?}"),
        })?;

        info!(target: LOG_LIGHTNING, "Gateway successfully synced with the chain");
        Ok(())
    }
}

/// When a port is specified in the Esplora URL, the esplora client inside LDK
/// node cannot connect to the lightning node when there is a trailing slash.
/// The `SafeUrl::Display` function will always serialize the `SafeUrl` with a
/// trailing slash, which causes the connection to fail.
///
/// To handle this, we explicitly construct the esplora URL when a port is
/// specified.
fn get_esplora_url(server_url: &SafeUrl) -> anyhow::Result<String> {
    // Esplora client cannot handle trailing slashes
    let host = server_url
        .host_str()
        .ok_or(anyhow::anyhow!("Missing esplora host"))?;
    let server_url = if let Some(port) = server_url.port() {
        format!("{}://{}:{}", server_url.scheme(), host, port)
    } else {
        server_url.to_string()
    };
    Ok(server_url)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct UserChannelId(pub ldk_node::UserChannelId);

impl PartialOrd for UserChannelId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UserChannelId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.0.cmp(&other.0.0)
    }
}

#[cfg(test)]
mod tests;
