# Keysat

**Keysat** is a self-hosted Bitcoin-paid software licensing server, designed to run as a [Start9](https://start9.com) 0.4.0.x service alongside [BTCPay Server](https://btcpayserver.org). One instance can sell, issue, validate, and revoke licenses for any number of software products you own.

> The repository directory is still called `licensing-service/` on disk for continuity with earlier revisions. The crate, the binary, the StartOS package id, and all user-visible strings use **Keysat**.

Every developer who uses this runs their own instance on their own hardware. There is no central authority, no shared database, and no dependency on anyone else's servers. Your keys, your products, your customers, your rules.

## What it does

- Exposes a REST API for selling and managing software licenses paid for in Bitcoin via BTCPay Server.
- Issues **Ed25519-signed license keys** that can be verified offline by any client with your server's public key — so downstream software doesn't break if your licensing server is briefly unreachable.
- Supports multiple products per instance, each with independent pricing and license pools.
- Supports closed-source, open-source-for-convenience, and open-core distribution models. The service doesn't care how you distribute source; it only validates keys against products.
- Optional per-license machine fingerprint binding with trust-on-first-use.
- Admin-gated endpoints for product management, manual license issuance (comps/press/testing), and revocation.

## Architecture in two minutes

```
┌──────────────┐       ┌──────────────────────┐       ┌──────────────┐
│ Buyer's      │──────▶│ licensing-service    │──────▶│ BTCPay Server│
│ browser      │       │   (this program)     │       │   (Start9)   │
└──────────────┘       └──────────────────────┘       └──────────────┘
        ▲                        │    ▲                      │
        │  license key           │    │  webhook             │
        │                        ▼    │                      │
        │                 ┌──────────────┐                   │
        └─────────────────│   SQLite     │◀──────────────────┘
          poll/status     │   licensing.db                   
                          └──────────────┘                   

Downstream software (e.g. another Start9 package you sell):
  on startup → POST /v1/validate { key, product_slug, fingerprint }
  → caches result, re-checks on reasonable cadence
```

1. Buyer `POST /v1/purchase { product: "my-app" }` → we create a BTCPay invoice, return its checkout URL.
2. Buyer pays via BTCPay. BTCPay fires a signed webhook at `POST /v1/btcpay/webhook` → we mark the invoice settled and issue a license row.
3. Buyer polls `GET /v1/purchase/:invoice_id` → once settled, response contains the signed `license_key` string.
4. Buyer installs the software. On startup the software calls `POST /v1/validate` to check revocation and bind itself to the installation.

## Why Ed25519-signed keys

Each license key is a compact, cryptographically signed envelope:

```
LIC1-<74-byte payload, base32>-<64-byte signature, base32>
```

The payload contains the product id, license id, issue time, an optional fingerprint hash, and a version byte. The server's private key signs it; anyone with the public key can verify it.

The practical benefit: downstream software can verify a key's signature **offline**, using a public key bundled at compile time. It only needs to reach your licensing server to check revocation, and it can cache that check. If your licensing server has an outage, existing installations keep working. If someone tries to forge a key, the signature fails instantly without a database lookup.

See [`src/crypto/mod.rs`](src/crypto/mod.rs) for the exact byte layout.

## Project layout

```
licensing-service/
├── Cargo.toml
├── LICENSE                        # source-available; no redistribution
├── README.md
├── .env.example                   # required env vars
├── migrations/
│   └── 0001_initial.sql           # SQLite schema
├── src/
│   ├── main.rs                    # entry point: wires everything
│   ├── config.rs                  # env-driven config
│   ├── error.rs                   # unified error → HTTP mapping
│   ├── models.rs                  # shared domain types
│   ├── crypto/
│   │   ├── mod.rs                 # license key format + sign/verify
│   │   └── keys.rs                # server keypair lifecycle
│   ├── db/
│   │   ├── mod.rs                 # pool + migrations
│   │   └── repo.rs                # all SQL queries
│   ├── btcpay/
│   │   ├── client.rs              # Greenfield API client
│   │   └── webhook.rs             # HMAC verification + event parsing
│   └── api/
│       ├── mod.rs                 # router + AppState
│       ├── products.rs            # public product endpoints
│       ├── purchase.rs            # buy + poll
│       ├── validate.rs            # the hot path for downstream software
│       ├── webhook.rs             # BTCPay landing
│       └── admin.rs               # operator-only actions
└── docs/
    ├── API.md                     # full endpoint reference
    ├── INTEGRATION.md             # for developers embedding a client
    └── ARCHITECTURE.md            # deeper design notes
```

## Running locally

Prerequisites: Rust 1.75+, a BTCPay Server instance you can point at (local or hosted).

```bash
cp .env.example .env
# edit .env — generate admin key with: openssl rand -hex 32
# fill in BTCPay URL, API key, store id, webhook secret

cargo run --release
```

On first boot the server generates a fresh Ed25519 keypair and stores it in the SQLite database. Get the public key anytime from `GET /v1/pubkey` (or from the logs on first boot).

### Creating your first product

```bash
curl -X POST http://localhost:8080/v1/admin/products \
  -H "Authorization: Bearer $LICENSING_ADMIN_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "slug": "my-app",
    "name": "My App",
    "description": "A cool Start9 service.",
    "price_sats": 50000
  }'
```

### Walking through a purchase

```bash
# 1. Buyer starts a purchase
curl -X POST http://localhost:8080/v1/purchase \
  -H "Content-Type: application/json" \
  -d '{"product": "my-app"}'
# → { "invoice_id": "...", "checkout_url": "https://btcpay.../i/...", ... }

# 2. Buyer opens checkout_url, pays

# 3. Buyer polls
curl http://localhost:8080/v1/purchase/<invoice_id>
# → { "status": "settled", "license_key": "LIC1-...", ... }

# 4. Downstream software validates the key
curl -X POST http://localhost:8080/v1/validate \
  -H "Content-Type: application/json" \
  -d '{"key": "LIC1-...", "product_slug": "my-app", "fingerprint": "host-abc123"}'
# → { "ok": true, "license_id": "...", "product_id": "..." }
```

## Deploying on Start9

This repository ships the service only. To package as an `.s9pk` for the 0.4.0.x platform you'll need a separate wrapper repository following [docs.start9.com/packaging/0.4.0.x](https://docs.start9.com/packaging/0.4.0.x/). The service is designed to slot in cleanly:

- **Declares a dependency** on BTCPay Server in the manifest. StartOS will make BTCPay reachable at a `.startos` hostname and supply the env vars from the wrapper's action handlers.
- **Persists to `/data`**, so everything (SQLite DB including the signing key) is covered by one-click encrypted backups.
- **Binds to `0.0.0.0:8080`** and expects StartOS to handle Tor/LAN/clearnet exposure.
- **Graceful shutdown** on SIGTERM, as StartOS expects.
- **Environment-driven config**, no config files needed at runtime.

When you're ready to write the manifest, the env vars you need to wire are listed in `.env.example`. The main gotcha is the BTCPay webhook secret: you configure it on the BTCPay side and it must match `BTCPAY_WEBHOOK_SECRET` exactly — we verify HMAC-SHA256 in constant time and reject any mismatch.

## Developer integration

If you're a developer shipping software that should validate against a licensing-service instance, see [`docs/INTEGRATION.md`](docs/INTEGRATION.md). It covers:

- Bundling the server's public key in your client.
- Offline signature verification + online revocation check.
- Graceful handling of server outages (don't brick your users).
- Recommended caching and rate-limiting patterns.

## Source-available licensing

This project is source-available, not open source. You may read, audit, self-host, and modify for your own use, but may not redistribute, resell, or publicly host for others. See [LICENSE](LICENSE) for the full terms.

Commercial redistribution / resale rights: contact licensing@keysat.xyz.

## Status

v0.1 — minimal working implementation. Feature direction after this is expected to cover: SDK crates for Rust and TypeScript, s9pk wrapper repository, richer admin UI, invoice reconciliation job for dropped webhooks, per-product webhook endpoints for the operator.
