//! Interest-driven probing: periodically probes a configured list of concrete
//! destinations to keep the pathfinding scorer warm for the nodes we actually
//! pay.
//!
//! This complements ldk-node's built-in high-degree prober (configured in
//! `main`), which probes the network's most-connected hubs rather than the
//! specific destinations our users route to. A probe reaches a node by public
//! key through the gossip graph and never dials it, so no socket address is
//! needed — the operator supplies a plain list of pubkeys via
//! `FM_INTEREST_PROBE_NODES`.

use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use fedimint_core::runtime::sleep;
use fedimint_logging::LOG_GATEWAY;
use rand::Rng as _;
use rand::seq::SliceRandom as _;
use tracing::warn;

use crate::AppState;

/// Periodically probes a random node from `nodes` to keep the pathfinding
/// scorer warm for destinations we care about, at a random amount within
/// `[min_amount_msat, max_amount_msat]`. Unlike the built-in high-degree prober
/// this targets concrete nodes we actually pay. Probe outcomes are only
/// observable in the LDK logs (the `Got route` / `Onion Error` lines); nothing
/// is timed here. Spawned from `main` only when `nodes` is non-empty.
pub async fn run_interest_prober(
    state: AppState,
    nodes: Vec<PublicKey>,
    interval_secs: u64,
    min_amount_msat: u64,
    max_amount_msat: u64,
) {
    loop {
        // Fresh `thread_rng()` temporaries so no `!Send` rng is held across the
        // `.await` below; `nodes` is non-empty by the spawn-time guard.
        let node_id = *nodes
            .choose(&mut rand::thread_rng())
            .expect("prober is only spawned with a non-empty node list");

        let amount_msat = rand::thread_rng().gen_range(min_amount_msat..=max_amount_msat);

        if let Err(err) = state
            .node
            .spontaneous_payment()
            .send_probes(amount_msat, node_id)
        {
            warn!(target: LOG_GATEWAY, %node_id, ?err, "Failed to dispatch interest probe");
        }

        sleep(Duration::from_secs(interval_secs)).await;
    }
}
