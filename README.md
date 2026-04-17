# Picomint

A minimalist fork of [Fedimint](https://github.com/fedimint/fedimint). Picomint keeps the core Chaumian e-cash mint and Lightning integration but strips every pluggability layer, trait tower, setup flow, legacy module, and deployment knob that was not strictly required. The result is a small, deployment-focused codebase with two binaries (a federation guardian and a Lightning gateway) that you can ship via Docker in a single `docker-compose up`.

Picomint runs over [Iroh](https://iroh.computer/) (QUIC + hole-punching), uses [redb](https://github.com/cberner/redb) for storage, and ships without migrations, backups, or version negotiation.

> ⚠️ **Beta / experimental.** Not recommended for real funds.

## Features

- **Two binaries, two roles** — `picomint-server-daemon` (federation guardian) and `picomint-gateway-daemon` (Lightning gateway with embedded LDK node).
- **Iroh-native networking** — no public IP, domain, or TLS configuration required.
- **Static module set** — mint, wallet, lightning (v2 only). No dyn-module plumbing.
- **redb storage** — single file per daemon, no migrations.
- **Admin CLIs shipped in the container** — `picomint-server-cli` and `picomint-gateway-cli` reach the daemon over a localhost-only HTTP socket.

## Deploy a federation guardian

Download the reference compose file:

```bash
curl -O https://raw.githubusercontent.com/joschisan/picomint/main/docker-server/docker-compose.yml
```

Edit `UI_PASSWORD` to a strong password, then:

```bash
docker-compose up -d
```

Day-to-day admin actions (including DKG setup) go through `picomint-server-cli`, running inside the container:

```bash
docker exec -it picomint-server picomint-server-cli setup status
```

### Optional: Web UI

The Web UI is off by default. To enable it, uncomment `UI_ADDR` and `UI_PASSWORD` in `docker-compose.yml` (plus the `127.0.0.1:3000:3000` port mapping) and restart the container. Then forward it over SSH if the daemon runs remotely:

```bash
ssh -NL 3000:127.0.0.1:3000 <your_server>
```

Never expose the UI to the public internet without TLS — run it behind a reverse proxy that terminates HTTPS.

## Deploy a Lightning gateway

```bash
curl -O https://raw.githubusercontent.com/joschisan/picomint/main/docker-gateway/docker-compose.yml
docker-compose up -d
```

## Admin CLI

Both CLIs are included in their respective images and available on the container `PATH`. Open a shell inside the container:

```bash
docker exec -it picomint-server bash
picomint-server-cli --help
```

Or run commands one-shot:

```bash
docker exec picomint-server picomint-server-cli invite
docker exec picomint-gateway picomint-gateway-cli info
```

The admin CLI speaks over a Unix socket inside `DATA_DIR`, never over TCP. `docker exec` reaches it from inside the container; the host and network cannot.

## Interfaces

### Server daemon (`picomint-server-daemon`)

| Port | Purpose                      | Safe to expose? |
|------|------------------------------|-----------------|
| 8080 | Iroh endpoint                | Yes             |
| 3000 | Web UI (setup + dashboard)   | Localhost only  |

The admin CLI is a Unix socket at `{DATA_DIR}/cli.sock` — no port, no
network exposure. Reach it with `docker exec -it picomint-server
picomint-server-cli …`.

### Gateway daemon (`picomint-gateway-daemon`)

| Port | Purpose                      | Safe to expose? |
|------|------------------------------|-----------------|
| 8080 | Public API (HTTP)            | Yes             |
| 9735 | LDK Lightning P2P (BOLT)     | Yes             |

The admin CLI is a Unix socket at `{DATA_DIR}/cli.sock` — same pattern
as the server daemon.

## Configuration reference

### Server daemon

| Env                          | Required | Default           | Description                                |
|------------------------------|----------|-------------------|--------------------------------------------|
| `DATA_DIR`                   | yes      |                   | Directory for the redb database file       |
| `BITCOIN_NETWORK`            | yes      | `regtest`         | `bitcoin`, `testnet`, `signet`, `regtest`  |
| `ESPLORA_URL`                | one of   |                   | Esplora HTTP URL, e.g. `https://mempool.space/api` |
| `BITCOIND_URL`               | one of   |                   | Bitcoin Core RPC URL                       |
| `BITCOIND_USERNAME`          | if RPC   |                   | Bitcoin Core RPC user                      |
| `BITCOIND_PASSWORD`          | if RPC   |                   | Bitcoin Core RPC password                  |
| `P2P_ADDR`                   | no       | `0.0.0.0:8080`    | Iroh endpoint listen address               |
| `UI_ADDR`                    | no       |                   | Web UI listen address — unset disables UI  |
| `UI_PASSWORD`                | if UI    |                   | Web UI password, required when `UI_ADDR` is set |

*Either `ESPLORA_URL` or `BITCOIND_URL` must be set, but not both.*

### Gateway daemon

| Env                        | Required | Default           | Description                                 |
|----------------------------|----------|-------------------|---------------------------------------------|
| `DATA_DIR`                 | yes      |                   | Directory for redb + LDK node data          |
| `BITCOIN_NETWORK`          | yes      |                   | Bitcoin network the gateway runs on         |
| `ESPLORA_URL`              | one of   |                   | Esplora HTTP URL                            |
| `BITCOIND_URL`             | one of   |                   | Bitcoin Core RPC URL                        |
| `BITCOIND_USERNAME`        | if RPC   |                   | Bitcoin Core RPC user                       |
| `BITCOIND_PASSWORD`        | if RPC   |                   | Bitcoin Core RPC password                   |
| `API_ADDR`                 | no       | `0.0.0.0:8080`    | Public API listen address                   |
| `LDK_ADDR`                 | no       | `0.0.0.0:9735`    | LDK Lightning P2P listen address (BOLT)     |
| `ROUTING_FEE_BASE_MSAT`    | no       | `2000`            | Lightning base routing fee (msat)           |
| `ROUTING_FEE_PPM`          | no       | `3000`            | Lightning routing fee rate (ppm)            |
| `TRANSACTION_FEE_BASE_MSAT`| no       | `2000`            | Federation transaction base fee (msat)      |
| `TRANSACTION_FEE_PPM`      | no       | `3000`            | Federation transaction fee rate (ppm)       |

## Server CLI

```bash
picomint-server-cli setup status
picomint-server-cli setup set-local-params <name> [--federation-name X] [--federation-size N]
picomint-server-cli setup add-peer <setup-code>
picomint-server-cli setup start-dkg

picomint-server-cli invite
picomint-server-cli audit

picomint-server-cli module wallet total-value
picomint-server-cli module wallet block-count
picomint-server-cli module wallet feerate
picomint-server-cli module wallet tx-chain
picomint-server-cli module wallet pending-tx-chain

picomint-server-cli module ln gateway add <url>
picomint-server-cli module ln gateway remove <url>
picomint-server-cli module ln gateway list
```

## Gateway CLI

```bash
picomint-gateway-cli info
picomint-gateway-cli mnemonic

picomint-gateway-cli ldk balances
picomint-gateway-cli ldk onchain receive
picomint-gateway-cli ldk onchain send --address <addr> --amount <amt> --sats-per-vbyte <n>
picomint-gateway-cli ldk channel open <pubkey> <host> <channel-size-sats> [--push-amount-sats N]
picomint-gateway-cli ldk channel close <pubkey> [--force] [--sats-per-vbyte N]
picomint-gateway-cli ldk channel list
picomint-gateway-cli ldk peer connect <pubkey> <host>
picomint-gateway-cli ldk peer disconnect <pubkey>
picomint-gateway-cli ldk peer list
picomint-gateway-cli ldk invoice create <amount-msats> [--expiry-secs N] [--description S]
picomint-gateway-cli ldk invoice pay <bolt11>

picomint-gateway-cli federation join <invite>
picomint-gateway-cli federation list
picomint-gateway-cli federation config <federation-id>
picomint-gateway-cli federation invite <federation-id>
picomint-gateway-cli federation balance <federation-id>

picomint-gateway-cli module <federation-id> mint count
picomint-gateway-cli module <federation-id> mint send <amount>
picomint-gateway-cli module <federation-id> mint receive <ecash>
picomint-gateway-cli module <federation-id> wallet receive
picomint-gateway-cli module <federation-id> wallet send <address> <amount> [--fee F]
picomint-gateway-cli module <federation-id> wallet send-fee
```

## License

MIT. Derived from [Fedimint](https://github.com/fedimint/fedimint).
