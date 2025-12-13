# Recurringdv2

`recurringdv2` is a stateless lnurl proxy service that allows Fedimint users to receive lnurl payments via lnv2.

This service requires no database or persistent state. All payment information is encrypted and embedded in the LNURL itself, making it easy to deploy on platforms like Digital Ocean App Platform, Fly.io, Railway, etc.

The operator of the service is trusted to provide the correct invoice to the requester, but does not take custody of the funds when the invoice is paid.

## How it works

1. Client calls `POST /lnurl` with payment details (federation ID, recipient public key, gateways, etc.)
2. Server encrypts this payload and returns an LNURL containing the encrypted data
3. When a payer scans the LNURL, `GET /pay/{payload}` returns the LNURL-pay response
4. Payer requests invoice via `GET /invoice/{payload}?amount=X`
5. Server decrypts payload, creates an incoming contract with a gateway, and returns a BOLT11 invoice
6. Payer pays the invoice directly to the gateway
7. Recipient claims funds from the federation when they come online

Note that once the invoice is generated, `recurringdv2` cannot claim the funds for itself.

## Command line options

```text
Usage: fedimint-recurringdv2 --base-url <BASE_URL> --encryption-key <ENCRYPTION_KEY>

Options:
      --port <PORT>                        [env: PORT=] [default: 8176]
      --base-url <BASE_URL>                [env: BASE_URL=]
      --encryption-key <ENCRYPTION_KEY>    [env: ENCRYPTION_KEY=]
  -h, --help                               Print help
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Health check |
| POST | `/lnurl` | Register a new LNURL (accepts `LnurlRequest` JSON) |
| GET | `/pay/{payload}` | LNURL-pay first step (returns `PayResponse`) |
| GET | `/invoice/{payload}?amount=X` | LNURL-pay second step (returns invoice) |

### Environment Variables

- `PORT` - Port to listen on (default: `8176`)
- `BASE_URL` - Public URL of the service (used in LNURL callbacks)
- `ENCRYPTION_KEY` - Secret key for encrypting/decrypting payloads
