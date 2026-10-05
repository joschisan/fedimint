# gatewaydv2

The gateway is a single container image, built by CI on every push to the
`gatewayv2` branch: `ghcr.io/joschisan/gatewaydv2:latest`, or `:<sha>` for a
reproducible deploy. `docker-compose.yml` in this directory runs it beside a
pruned bitcoind and a Caddy reverse proxy for the public client API: persist
`/data` in a volume, publish the LDK Lightning P2P port `9735` so peers can
connect, and configure it through the environment variables the compose file
documents. `FM_NETWORK` is required and has to be the network of every
federation the gateway serves.

## Accessing the CLI

The `gatewaydv2-cli` binary is included in the container and on the `PATH`,
and reads `FM_DATA_DIR` from the same environment as the daemon. Run CLI
commands from the host like:

```bash
docker exec gatewaydv2 gatewaydv2-cli --help
```

The commands below use the bare `gatewaydv2-cli …` form; prefix with
`docker exec gatewaydv2` to run them. Every command prints JSON, and its
`--help` ends with the JSON Schema of what it prints, every field explained.

A refused request prints one JSON object on stderr,
`{"code": ..., "error": ...}`: the code is a stable name to branch on, the
error the message to show the operator. The exit code is 1 for a request the
daemon refused, 2 for a usage error and 3 when the daemon is unreachable.
Every command's `--help` lists the codes it fails with.

One command prints a secret: `mnemonic` prints the seed words the LDK
onchain wallet and every federation balance derive from. Whatever an agent
reads ends up in a model context and a transcript, so the rules for an agent
are: run it only when asked, always with the output piped into a file, never
read the file, and open it for the operator if asked. Write the words down
from that file yourself and delete it. The same rules end the CLI's
`--help`. The shell creates that file with its umask, world-readable on most
systems, so create it in a subshell with `umask 077` and it is readable by
you alone from the first byte:

```bash
(umask 077; gatewaydv2-cli mnemonic > mnemonic.json)
```

A first call to confirm everything is wired up:

```bash
gatewaydv2-cli info
```

## Analytics

The gateway mirrors the gwv2 payment events of every federation client's
event log into a SQLite database at `{FM_DATA_DIR}/analytics/analytics.sqlite`.
The directory is wiped on every startup and rebuilt by replaying the event
logs, so it is derived state, safe to delete.

Query it with read-only SQL through the admin CLI; rows print as JSON
objects keyed by column name:

```bash
gatewaydv2-cli query "SELECT * FROM outgoing_payments ORDER BY started_at DESC LIMIT 10"
```

`query --help` prints the schema as the SQL that creates it, with no daemon
running: one table per event, and the `outgoing_payments` and
`incoming_payments` views that join them into one row per payment.
