# gatewaydv2

A Fedimint Lightning gateway scoped to the LDK lightning backend and the LNv2
module. This crate builds the `gatewaydv2` daemon; its admin CLI
`gatewaydv2-cli` lives in the sibling `fedimint-gatewayv2-cli` crate.

## Deploy

Pick a stable directory for the deployment:

```bash
mkdir -p ~/gatewaydv2 && cd ~/gatewaydv2
```

Download the compose file:

```bash
curl -O https://raw.githubusercontent.com/fedimint/fedimint/master/docker/gatewaydv2/docker-compose.yml
```

And then run:

```bash
sudo docker compose up -d
```

The bundled compose uses a public Esplora instance as the chain-data source;
to run against your own Bitcoin Core node instead, see
[Configuration](#configuration) below.

## Accessing the CLI

The `gatewaydv2-cli` binary is included in the container and on the `PATH`.
Open an interactive shell inside the container via:

```bash
sudo docker exec -it gatewaydv2 bash
```

Or run CLI commands directly from the host like:

```bash
sudo docker exec gatewaydv2 gatewaydv2-cli --help
```

The walkthroughs below use the bare `gatewaydv2-cli …` form. Run them from
inside the container shell, or prefix with `sudo docker exec gatewaydv2` to
run from the host.

A first call to confirm everything is wired up:

```bash
gatewaydv2-cli info
```

Your info will look like

```json
{
  "lightning_pk": "02abfe4a99f1ed8f67c1f07e5d47f3ab3d2e9c5b8a1c8e7f2a6d4b7e9c1f5a3e8d",
  "network": "bitcoin",
  "block_height": 842195,
  "synced_to_chain": true
}
```

## Open Channels

To route payments on behalf of federations the gateway needs Lightning
channels — specifically inbound liquidity, since a fresh node cannot receive
payments. The usual approach is to buy an inbound channel from a Lightning
Service Provider (LSP) such as [LN Big](https://lnbig.com). LSPs will ask for
the node's `lightning_pk` from `info` above and may require you to connect to
them before they open the channel:

```bash
gatewaydv2-cli ldk peer connect <lsp-pubkey> <lsp-host>
```

You can also open outbound channels yourself but first the gateway's embedded
LDK node needs onchain bitcoin to open channels. Generate a receive address:

```bash
gatewaydv2-cli ldk onchain receive
```

Send bitcoin to it, then check the result:

```bash
gatewaydv2-cli ldk balances
```

Once the onchain balance is available connect to a node and open a channel with

```bash
gatewaydv2-cli ldk channel open <pubkey> <host> <channel-size-sat>
```

Running a second outbound channel alongside the LSP's inbound one is
worthwhile: with only one channel, outgoing payments can fail once user
balances drain toward the counterparty's channel reserve. Monitor channel
state with:

```bash
gatewaydv2-cli ldk channel list
```

## Join Federations

The gateway can serve multiple federations simultaneously. Join one with an
invite code:

```bash
gatewaydv2-cli federation join <invite>
```

List joined federations:

```bash
gatewaydv2-cli federation list
```

For the gateway to actually route payments on behalf of a federation, its
guardians also need to vouch for the gateway — clients prioritize gateways by
the number of guardians vouching for them. Each guardian adds the gateway's
public API URL via:

```bash
fedimint-cli --our-id <peer-id> --password <password> module lnv2 gateways add <url>
```

## Manage Federation Liquidity

Every per-federation command below takes the federation id (from
`federation list`) as its first argument.

The gateway holds its own ecash balance in every federation it has joined.
Check it with:

```bash
gatewaydv2-cli federation balance <federation-id>
```

You can move funds in and out either onchain or as an ecash string.

**Receive Onchain:** generate a federation deposit address and send bitcoin to
it. When the transaction confirms the federation mints ecash to the gateway.

```bash
gatewaydv2-cli federation module wallet receive <federation-id>
```

**Send Onchain:** burn ecash in exchange for an onchain transfer to the given
address. The federation picks a feerate; check what it will charge first:

```bash
gatewaydv2-cli federation module wallet send-fee <federation-id>
```

Then send:

```bash
gatewaydv2-cli federation module wallet send <federation-id> <address> <amount>
```

Passing `--fee <amount>` overrides the feerate with an exact value; otherwise
whatever `send-fee` currently reports is used.

**Send Ecash:** spend part of the federation balance as an ecash string you
can hand to another client:

```bash
gatewaydv2-cli federation module mint send <federation-id> <amount>
```

**Receive Ecash:** reissue an ecash string produced by `mint send` (on this
gateway or any other client) into your balance. The target federation is read
from the ecash itself:

```bash
gatewaydv2-cli federation module mint receive <ecash>
```

## Recovery

If your gateway deployment is ever corrupted you can recover your onchain
funds and ecash from your twelve word mnemonic:

```bash
gatewaydv2-cli mnemonic
```

The mnemonic can be used with any BIP 39 compatible wallet to recover the
onchain funds and with any Fedimint wallet to recover the funds in the
federations. **The balance in your open lightning channels is lost.**

## Analytics

The gateway mirrors every gw-module event into a SQLite database at
`{DATA_DIR}/analytics/analytics.sqlite`. The directory is **wiped on every
startup** and rebuilt by replaying the event logs — analytics are derived,
not authoritative, so it's safe to delete and let it rebuild.

Activity is exposed through two per-direction views, `outgoing_payments` and
`incoming_payments`. Each row is one payment, joined across the underlying
event tables. They are kept separate because the outgoing side carries an
LN-routing-fee budget that doesn't exist on the incoming side — a single
unified view would mean three permanent NULL columns on every incoming row.

Inspect the DB with `sqlite3` directly (the gateway container already has it
installed). Pass `-header -column` for human-readable, column-aligned output —
without it `sqlite3` prints unlabeled pipe-delimited rows. Ten most recent
outgoing payments:

```bash
sudo docker exec -it gatewaydv2 \
    sqlite3 -header -column /data/analytics/analytics.sqlite \
    "SELECT * FROM outgoing_payments ORDER BY started_at DESC LIMIT 10;"
```

Status breakdown for outgoing:

```bash
sudo docker exec -it gatewaydv2 \
    sqlite3 -header -column /data/analytics/analytics.sqlite \
    "SELECT status, COUNT(*) FROM outgoing_payments GROUP BY status;"
```

Total outgoing volume per federation, in sat:

```bash
sudo docker exec -it gatewaydv2 \
    sqlite3 -header -column /data/analytics/analytics.sqlite \
    "SELECT federation, SUM(amount_msat)/1000 AS sat \
     FROM outgoing_payments WHERE status='success' GROUP BY federation;"
```

**Columns common to both views:**

| Column          | Type    | Notes                                                       |
|-----------------|---------|-------------------------------------------------------------|
| `federation`    | TEXT    | Hex-encoded federation id                                   |
| `payment_image` | TEXT    | Hex-encoded payment image; unique within a federation       |
| `status`        | TEXT    | See below                                                   |
| `started_at`    | INTEGER | When the payment was initiated (ms since epoch)             |
| `completed_at`  | INTEGER | NULL while `status = 'pending'`                             |
| `direct`        | INTEGER | 1 if this is one side of a direct swap between federations  |
| `amount_msat`   | INTEGER | Payment amount, msat                                        |
| `gw_fee_msat`   | INTEGER | The gateway's fee cut                                       |
| `preimage`      | TEXT    | Hex-encoded; NULL unless `status = 'success'`               |
| `error`         | TEXT    | NULL unless the payment failed                              |

**Additional columns on `outgoing_payments`:**

| Column               | Type    | Notes                                                        |
|----------------------|---------|--------------------------------------------------------------|
| `ln_fee_budget_msat` | INTEGER | Maximum LN routing fee the gateway committed to pay          |
| `ln_fee_paid_msat`   | INTEGER | Realized LN routing fee (NULL while pending; 0 if cancelled) |
| `ln_fee_kept_msat`   | INTEGER | `ln_fee_budget_msat - ln_fee_paid_msat` (NULL while pending) |

**Status values:**

- `outgoing_payments`: `pending`, `success`, `cancelled`
- `incoming_payments`: `pending`, `success`, `failure`

A direct swap between two federations routes entirely inside the gateway and
writes both an outgoing and an incoming row with the same `payment_image`;
`direct` flags those rows so swap and external LN activity can be separated
or combined in aggregates. Since a swap never touches the Lightning network,
its LN fee budget, paid and kept are all zero.

The raw event tables (`send`, `send_success`, `send_cancel`, `receive`,
`receive_success`, `receive_failure`) are also queryable if you need a finer
view.

## Interfaces

| Port | Purpose                      | Safe to expose? |
|------|------------------------------|-----------------|
| 8080 | Public API (HTTP)            | Yes             |
| 9735 | LDK Lightning P2P (BOLT)     | Yes             |

The admin CLI is a Unix socket at `{DATA_DIR}/cli.sock` — no port, no network
exposure. Reach it with `sudo docker exec -it gatewaydv2 gatewaydv2-cli …`.

## Configuration

Every option is available both as an environment variable and as a command
line flag. Exactly one of `FM_BITCOIND_URL` and `FM_ESPLORA_URL` is required;
if both are set, bitcoind is used.

| Env                           | Required | Default          | Description                                 |
|-------------------------------|----------|------------------|---------------------------------------------|
| `FM_DATA_DIR`                 | yes      |                  | Directory for gateway db + LDK node data    |
| `FM_NETWORK`                  | no       | `bitcoin`        | `bitcoin`, `testnet`, `signet`, `regtest`   |
| `FM_BITCOIND_URL`             | one of   |                  | Bitcoin Core RPC URL with embedded credentials, e.g. `http://user:pass@127.0.0.1:8332` |
| `FM_ESPLORA_URL`              | one of   |                  | Esplora HTTP URL, e.g. `https://blockstream.info/api` |
| `FM_API_ADDR`                 | no       | `0.0.0.0:8080`   | Public API listen address                   |
| `FM_LDK_ADDR`                 | no       | `0.0.0.0:9735`   | LDK Lightning P2P listen address (BOLT)     |
| `FM_LDK_ALIAS`                | no       | `LDK Gateway`    | The LDK node's advertised alias             |
| `FM_DEFAULT_ROUTING_FEES`     | no       | `2000 msat,3000` | Lightning routing fee for new federations as `<base>,<ppm>` |
| `FM_DEFAULT_TRANSACTION_FEES` | no       | `2000 msat,3000` | Federation transaction fee for new federations as `<base>,<ppm>` |
