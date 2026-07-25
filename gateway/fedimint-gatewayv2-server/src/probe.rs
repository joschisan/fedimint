//! Well-known payment destinations a gateway is likely to pay.
//!
//! Scorer warming is handled by ldk-node's built-in probing service (the
//! high-degree strategy, configured in `main`), which probes the network's
//! most-connected hubs rather than a curated list. This list is kept around
//! as the candidate target set for a future custom probing strategy aimed at
//! concrete destinations.
//!
//! Unlike the seed-peer list in `connect`, these carry no socket address: a
//! probe is routed to a node by public key through the gossip graph and never
//! dials it, so an address is unnecessary — and some top destinations (e.g.
//! Binance) advertise only Tor, which an address-keyed list would exclude.

/// Popular Lightning destinations — `(alias, node_id)` — that a gateway is
/// likely to pay, probed to keep routes to them warm in the scorer.
pub const PROBE_TARGETS: &[(&str, &str)] = &[
    (
        "ACINQ (Phoenix)",
        "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
    ),
    (
        "Binance",
        "03a1f3afd646d77bdaf545cceaf079bab6057eae52c6319b63b5803d0989d6a72f",
    ),
    (
        "Bitfinex",
        "033d8656219478701227199cbd6f670335c8d408a92ae88b962c49d4dc0e83e025",
    ),
    (
        "Bitnob",
        "02e880b95c737bbe110c5ef4f5e70d0d4d04253503e42c4de2760d519f9cb9a81e",
    ),
    (
        "Bitrefill",
        "03d607f3e69fd032524a867b288216bfab263b6eaee4e07783799a6fe69bb84fac",
    ),
    (
        "Bitstamp",
        "02a04446caa81636d60d63b066f2814cbd3a6b5c258e3172cbdded7a16e2cfff4c",
    ),
    (
        "Blink",
        "02dfb4c1dd59216fa6a28d0f012e188516f63517db68c4e4b82c3af41343a05bc4",
    ),
    (
        "Block",
        "027100442c3b79f606f80f322d98d499eefcb060599efc5d4ecb00209c2cb54190",
    ),
    (
        "Breez",
        "0264a62a4307d701c04a46994ce5f5323b1ca28c80c66b73c631dbcb0990d6e835",
    ),
    (
        "Fedi",
        "02dd3fcdaa17b9bc83bf7138fbea85d0e83385a68b5fc8f9933658c8ee04644f68",
    ),
    (
        "Flashsats",
        "02e4971e61a3f55718ae31e2eed19aaf2e32caf3eb5ef5ff03e01aa3ada8907e78",
    ),
    (
        "Kraken",
        "02f1a8c87607f415c8f22c00593002775941dea48869ce23096af27b0cfdcc0b69",
    ),
    (
        "Machankura",
        "03202791105580f5b7869ebd599c4ae1ae2a3e1ef47213dfa060cb201e0e1b4505",
    ),
    (
        "Megalith",
        "038a9e56512ec98da2b5789761f7af8f280baf98a09282360cd6ff1381b5e889bf",
    ),
    (
        "OKX",
        "0294ac3e099def03c12a37e30fe5364b1223fd60069869142ef96580c8439c2e0a",
    ),
    (
        "Olympus (Zeus)",
        "031b301307574bbe9b9ac7b79cbe1700e31e544513eae0b5d7497483083f99e581",
    ),
    (
        "OpenNode",
        "03abf6f44c355dec0d5aa155bdbdd6e0c8fefe318eff402de65c6eb2e1be55dc3e",
    ),
    (
        "River",
        "03037dc08e9ac63b82581f79b662a4d0ceca8a8ca162b1af3551595b8f2d97b70a",
    ),
    (
        "Speed",
        "031df8ea711416b52d33c2f4a9b2a41d82f1da3c7672ffef2c24b0751cbdb75404",
    ),
    (
        "Strike",
        "03c8e5f583585cac1de2b7503a6ccd3c12ba477cfd139cd4905be504c2f48e86bd",
    ),
    (
        "Wallet of Satoshi",
        "035e4ff418fc8b5554c5d9eea66396c227bd429a3251c8cbc711002ba215bfc226",
    ),
    (
        "ZEBEDEE",
        "024096debaac318f5c671ee67dee0a8fe94ef0ad3fe86e2e6e825567637f89a968",
    ),
];
