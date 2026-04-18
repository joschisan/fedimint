# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Picomint is a minimalist fork of Fedimint — two binaries (federation guardian + Lightning gateway), Iroh networking, redb storage, static module set (mint, wallet, ln). No dyn modules, no migrations, no backup/recovery, no version negotiation, no legacy v1 modules. See README.md for deployment.

## Build and development

- `cargo check --workspace` — full workspace type check
- `cargo build --workspace` — build everything
- `cargo test --workspace` — run all tests
- `just clippy` / `just format` / `just final-check`
- `./test-integration.sh` — end-to-end integration test (requires Docker + bitcoind)

## Architecture

### Crates
- `picomint-core` — shared types, encoding, networking primitives, db traits
- `picomint-server-daemon` — federation guardian binary (consensus via AlephBFT); owns the concrete mint/ln/wallet server-side module code under `src/consensus/{mint,ln,wallet}/`
- `picomint-server-cli` — admin CLI for the server daemon (HTTP-over-localhost)
- `picomint-gateway-daemon` — Lightning gateway binary with embedded LDK node
- `picomint-gateway-cli` — admin CLI for the gateway daemon
- `picomint-client` — client library
- `picomint-client-module` — client module traits + per-module state machines
- `picomint-redb` — redb-based database layer
- `picomint-api-client` — client-side API transport (Iroh-only)
- `modules/picomint-{mint,wallet,ln,gw}-{common,client}` — per-module wire types and client-side logic

### Wire + storage
- Wire: client↔server uses the `Encodable`/`Decodable` traits from `picomint-core::encoding`
- Storage: redb only. No RocksDB. No migrations (types implement redb's `Key`/`Value` directly via macros in `picomint-redb`)
- Transport: Iroh-only (QUIC + hole-punching). No TLS/websocket/DNS announcements
- Each guardian binds exactly one iroh `Endpoint` (one secret key, one node id) for both federation p2p and the public client API; the accept loop demuxes by remote node-id (peer set → P2P path, otherwise → public API path).

### Admin CLIs
- Both CLIs are thin HTTP-over-Unix-socket clients. They POST JSON to the daemon's admin socket at `{DATA_DIR}/cli.sock` (`CLI_SOCKET_FILENAME` const in each `*-cli-core` crate). No network exposure; `docker exec` is how you reach them in a container deployment.
- Route constants live in `picomint-server-cli-core` / `picomint-gateway-cli-core`.
- Shared request/response types also live in the `*-cli-core` crates; daemon handlers live in `picomint-server-daemon/src/cli.rs` and `picomint-gateway-daemon/src/cli.rs`.

### Env vars
Env var names are unprefixed (puncture-style): `DATA_DIR`, `BITCOIN_NETWORK`, `BITCOIND_URL`, etc. No `FM_*` prefix. `*_ADDR` is the convention for listen-address vars (`P2P_ADDR`, `UI_ADDR`, `API_ADDR`, `LDK_ADDR`). Defined inline via clap `#[arg(env = "...")]`.

## Conventions

- Never `unwrap()` outside tests — use `expect("...")` with a message explaining why it can't fail.
- Prefer concrete types over dyn/trait-objects. This project aggressively replaced the Fedimint dyn module system with static typed module sets.
- No comments that explain WHAT code does — names and types already say it. Only comment non-obvious WHY.
- Prefer deleting code over preserving it — picomint is explicitly a simplification project.
