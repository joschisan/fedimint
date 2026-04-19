# Recurring Daemon

`picomint-recurring-daemon` is a stateless LNURL proxy service that allows picomint clients to receive LNURL payments via Lightning.

This service requires no database or persistent state. All payment information is encoded in the LNURL itself, making it easy to deploy on platforms like Digital Ocean App Platform, Fly.io, Railway, etc.

The operator of the service is trusted to provide the correct invoice to the requester, but does not take custody of the funds when the invoice is paid.

## How it works

1. Client generates an LNURL locally containing encoded payment details (federation ID, recipient public key, gateways, etc.)
2. When a payer scans the LNURL, `GET /pay/{payload}` returns the LNURL-pay response
3. Payer requests invoice via `GET /invoice/{payload}?amount=X`
4. Server decodes payload, creates an incoming contract with a gateway, and returns a BOLT11 invoice
5. Payer pays the invoice directly to the gateway
6. Recipient claims funds from the federation when they come online

Note that once the invoice is generated, `recurringd` cannot claim the funds for itself.

## Command line options

```text
Usage: picomint-recurring-daemon [OPTIONS]

Options:
      --api-addr <API_ADDR>  Public HTTP API listen address [env: API_ADDR=] [default: 0.0.0.0:8080]
  -h, --help                 Print help
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Health check |
| GET | `/pay/{payload}` | LNURL-pay first step (returns `PayResponse`) |
| GET | `/invoice/{payload}?amount=X` | LNURL-pay second step (returns invoice) |

### Environment Variables

- `API_ADDR` - Public HTTP API listen address (default: `0.0.0.0:8080`)
