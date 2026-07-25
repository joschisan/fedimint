//! Default public nodes the gateway seeds connections to on first boot.

/// Large, well-run public nodes — `(alias, node_id, address)` — that a fresh
/// gateway seeds persisted connections to so it participates in the network
/// right away.
pub const PUBLIC_NODES: &[(&str, &str, &str)] = &[
    (
        "ACINQ",
        "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
        "3.33.236.230:9735",
    ),
    (
        "Block",
        "027100442c3b79f606f80f322d98d499eefcb060599efc5d4ecb00209c2cb54190",
        "3.230.33.224:9735",
    ),
    (
        "Strike",
        "03c8e5f583585cac1de2b7503a6ccd3c12ba477cfd139cd4905be504c2f48e86bd",
        "34.73.189.183:9735",
    ),
    (
        "Megalith",
        "038a9e56512ec98da2b5789761f7af8f280baf98a09282360cd6ff1381b5e889bf",
        "64.23.162.51:9735",
    ),
];
