# Minimint Design Document

A simplified, opinionated fork of Fedimint. Strips the codebase to its essential core: v2 modules, LDK-only gateway, iroh-only federation networking. Targets ~50-55k lines down from ~157k.

## Removals

### Modules
- Remove v1 modules (mint, wallet, ln) - replaced by v2 equivalents
- Remove meta module

### Gateway
- Remove LND support - LDK only, integrated directly (no `ILnRpcClient` trait)
- Remove gateway UI (separate crate, separate port - see Admin API below)
- Remove gateway iroh P2P - HTTP only
- Remove gateway backup/restore - mnemonic is the backup
- Remove gateway setup flow - generate mnemonic on first start, always running
- Remove ecash spend/receive, BOLT12 offers, payment summary/stats from admin API
- Remove federation manager complexity - keep connect/leave but simplify state

### Client
- Remove fedimint-cli entirely - no client-side CLI
- Remove WASM support and all `#[cfg(target_family = "wasm")]` conditionals
- Remove WebSocket support - iroh only for federation API
- Remove `AmountUnit` - everything is bitcoin (msats)
- Remove client-rpc crate
- Remove UniFFI bindings

### Server
- Remove single guardian support - minimum 4 guardians
- Remove `test_client_config_change_detection`, `test_guardian_password_change`

### Infrastructure
- Remove fedimint-recoverytool (v2 replacement exists)
- Remove recurringd v1 (v2 replacement exists)
- Remove load-test-tool
- Remove dbtool
- Remove backwards compatibility tests
- Remove latency tests
- Remove Start9 workflows
- Remove WASM build toolchain from flake.nix
- Remove cross-compilation targets (Android, iOS) unless needed

### Testing Framework
- Remove `fedimint-testing` and `fedimint-testing-core` crates
- Remove dummy, empty, unknown modules (only used by fixture tests)
- Remove fixture-based test framework entirely

## Architecture Changes

### Gateway: Direct LDK Integration

Remove the `fedimint-lightning` crate entirely. No `ILnRpcClient` trait - the gateway embeds LDK node directly:
- LDK node initialized from BIP39 mnemonic (same as gateway mnemonic)
- No trait indirection, no `LnRpcTracked` metrics wrapper boilerplate
- Direct method calls instead of `lightning_context.lnrpc.pay()`
- Gateway state machine simplified: just "starting" and "running"
- Zero setup procedure - generate mnemonic on first boot, expose via admin API
- ~20,000 lines → ~3,500-4,000 lines

### Admin API: Trait-based Dependency Injection

Both fedimintd and gatewayd already expose admin functionality via a dashboard trait (`IDashboardApi` for guardians, inherent methods on `Gateway`). This pattern is extended to replace the CLI entirely:

- **JSON REST API** served by the daemon on a localhost-only port (no auth needed)
- **Optional Web UI** on a separate localhost port consuming the same trait
- Modules contribute admin methods via the dashboard trait (e.g., lnv2 add/remove gateway calls the module's methods directly in-process)
- No dedicated CLI binary - operators use `curl` or any HTTP client
- fedimintd and gatewayd can be built with `--features ui` to include the web dashboard

This follows the same pattern as the [puncture](../puncture) project where `puncture-daemon` exposes:
- A local admin HTTP API (port 8082) for CLI-style operations
- A separate admin UI (port 8083) for dashboard access
- Both consume the same underlying daemon state
- `puncture-cli` is just a thin HTTP client wrapping `reqwest` calls to the admin API

### Testing: Single Federation, In-Process Clients

Redesign the test infrastructure following the [puncture-testing](../puncture/puncture-testing) approach where:
- Server daemons are **spawned as real processes** (real binaries)
- Clients are used **in-process** via the Rust SDK (no CLI wrapper, no JSON parsing)
- Admin operations use **CLI subprocess calls** to the daemon's admin API
- A **single test binary** orchestrates the full integration test

Concrete design:

**Environment setup** - one shared federation, hardcoded ports (no dynamic allocation):
```rust
// minimint-testing/src/main.rs
async fn main() -> Result<()> {
    let bitcoind = start_bitcoind(18443).await?;
    let esplora = start_esplora(50000, &bitcoind).await?;

    // Spawn 4 guardian processes (real fedimintd binaries)
    let guardians = start_federation(&[18174, 18176, 18178, 18180]).await?;

    // Spawn 2 LDK gateway processes (real gatewayd binaries)
    let gw1 = start_gateway(28175).await?;
    let gw2 = start_gateway(28177).await?;

    // Admin: connect gateways via CLI subprocess
    gateway_cli(&gw1, "connect-fed", &invite_code).await?;
    gateway_cli(&gw2, "connect-fed", &invite_code).await?;

    // Clients: in-process, direct Rust library calls
    let env = TestEnv { bitcoind, guardians, gw1, gw2, invite_code };

    // Run all module tests sequentially against shared federation
    fedimint_mintv2_tests::run_tests(&env).await?;
    fedimint_walletv2_tests::run_tests(&env).await?;
    fedimint_lnv2_tests::run_tests(&env).await?;
    gateway_tests::run_tests(&env).await?;
    core_tests::run_tests(&env).await?;

    Ok(())
}
```

**Module test crates** export functions instead of being standalone binaries:
```rust
// fedimint-lnv2-tests/src/lib.rs
pub async fn run_tests(env: &TestEnv) -> Result<()> {
    let client = env.new_client().await?;  // in-process fedimint-client
    let lnv2 = client.get_first_module::<LightningClientModule>()?;

    // Direct Rust API calls, no CLI, no JSON parsing
    let invoice = lnv2.create_invoice(sats(1000)).await?;
    lnv2.pay_invoice(invoice).await?;
    // ...
    Ok(())
}
```

**Backcompat testing** - the test binary links the current client library but can run against any server version:
```bash
# Normal CI
cargo run -p minimint-testing

# Backcompat: current client against old servers
FEDIMINTD_BIN=./bins/v0.11/fedimintd \
GATEWAYD_BIN=./bins/v0.11/gatewayd \
cargo run -p minimint-testing
```

This eliminates:
- The fixture test framework (`fedimint-testing`, `fedimint-testing-core`, dummy/empty/unknown modules)
- The `Client` CLI wrapper in devimint (~800 lines of shelling out to fedimint-cli)
- Dynamic port allocation (`vars.rs`, `net_overrides.rs`)
- Version-checking functions (`supports_lnv2()`, `supports_mint_v2()`, etc.)
- The `DevJitFed` lazy initialization machinery

Devimint: ~9,200 lines → ~1,500 lines.

### CI

- Keep self-hosted runners
- Single main workflow: lint, build, test
- 23 test functions (down from 49)
- ~17 federation instances (down from 39), potentially reducible to 2-3 with shared federation approach
- Nix + Cachix for reproducible builds and caching
- Remove backwards compatibility, upgrade, hourly, daily audit workflows initially

## Line Count Estimate

| Category | Current | After | Saved |
|----------|---------|-------|-------|
| V1 modules | 32,840 | 0 | 32,840 |
| Gateway | 20,000 | 3,750 | 16,250 |
| Testing framework | 5,000 | 0 | 5,000 |
| Devimint | 9,200 | 1,500 | 7,700 |
| Tools (recoverytool, recurringd, load-test, client-rpc, dbtool) | 5,500 | 0 | 5,500 |
| CLI | 2,800 | 0 | 2,800 |
| WASM/WS/UniFFI | 750 | 0 | 750 |
| Core cleanups (AmountUnit, single guardian, scattered v1) | - | - | 1,000 |
| Operation log + module version negotiation | 1,150 | 0 | 1,150 |
| **Total** | **~157,000** | **~84,000** | **~73,000** |

### What remains (~84k)
- fedimint-core: ~15,500 (types, encoding, config, module system, DB, minus version negotiation)
- fedimint-server: ~9,000 (consensus engine, AlephBFT, DKG, API)
- fedimint-client + client-module: ~13,800 (state machines, minus oplog)
- V2 modules (lnv2, mintv2, walletv2, gwv2-client): ~14,000
- Gateway: ~3,750
- Devimint + test code: ~6,500
- fedimintd + connectors + rocksdb + logging + eventlog + metrics + derive + startos + lnurl: ~9,000
- Other (nix, shell scripts, CI): ~5,000

## Decided

- **Event log**: Keep. Will be used in integration tests for asserting on system behavior (same pattern as puncture's event-driven test assertions).
- **Operation log**: Remove. The `oplog.rs` (378 lines) and related client operation tracking machinery is unnecessary complexity. State machine recovery can be handled differently.
- **Consensus encoding**: Keep for everything. Deterministic encoding is required for consensus and using one encoding everywhere is simpler than maintaining two.
- **Module version negotiation**: Remove entirely (477 lines in `fedimint-core/src/module/version.rs`). We control all deployments, no need for API version negotiation between clients and servers.

## Open Questions

- Can fedimint-client + fedimint-client-module merge to reduce abstraction?
- Exact approach for the trait-object based backcompat testing with mixed in-process/external nodes
