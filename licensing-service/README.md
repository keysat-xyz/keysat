# Keysat

**Keysat** is a Bitcoin-native, self-hosted licensing service for software creators, designed to run as a [Start9](https://start9.com) 0.4.0.x service alongside [BTCPay Server](https://btcpayserver.org) (or [Zaprite](https://zaprite.com) for Bitcoin + cards). One instance can sell, issue, validate, and revoke licenses for any number of software products you own.

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

The daemon source lives under `src/`, organized by subsystem (browse it for the current layout — the tree below has grown well past the v0.1 snapshot):

- `main.rs`, `config.rs`, `error.rs`, `models.rs` — entry point, env-driven config, error → HTTP mapping, shared domain types.
- `crypto/` — the LIC1 license-key byte layout and Ed25519 sign/verify (the contract the four SDKs implement).
- `db/` — SQLite pool, migrations, and `repo.rs` (all SQL). `migrations/` holds the numbered, additive schema (0001 through the latest; the schema has grown substantially since 0001).
- `payment/` (`btcpay/`, `zaprite/`) + `merchant_profiles.rs` — the payment-provider abstraction and multi-profile routing.
- `reconcile.rs`, `subscriptions.rs`, `upgrades.rs` — the background worker (invoice reconciliation, recurring renewals, tier upgrades).
- `api/` — the ~30 route modules: public (`products`, `purchase`, `validate`, `redeem`) and admin (`admin*`, scoped API keys, webhooks, etc.), plus the router and `AppState` in `api/mod.rs`.
- `web/index.html` — the embedded admin SPA.

Deeper docs: [`docs/API.md`](docs/API.md), [`docs/INTEGRATION.md`](docs/INTEGRATION.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Running locally

Prerequisites: Rust 1.88+ (the build toolchain; the crate's Cargo.toml still declares MSRV 1.75, but the dependency tree now requires a newer compiler), a BTCPay Server instance you can point at (local or hosted).

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
  -H "Authorization: Bearer $KEYSAT_ADMIN_API_KEY" \
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

The StartOS wrapper lives in **this same repository** under `../startos/` (this `licensing-service/` directory is the daemon source it bundles). Build the `.s9pk` for the 0.4.0.x platform from the parent directory — see the build/release guide and `../Makefile`. The service is designed to slot in cleanly:

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

0.2.0 — shipped and in production. The current feature set:

- **Four published SDKs** — TypeScript (npm), Rust (crates.io), Python (PyPI), and Go — all wire-compatible against the cross-check fixtures in `tests/crosscheck/`.
- **StartOS wrapper included in this repo** under `../startos/`; build the `.s9pk` from the parent directory (no separate wrapper repository).
- **Embedded admin SPA** (`web/index.html`) for all day-to-day operations.
- **Subscriptions** (recurring auto-renew with trials + grace), **policies / tiers** with per-policy entitlements, **discount / referral / free-license codes**, **outbound webhooks** with a dead-letter queue, and a background **invoice reconciliation** job that recovers dropped payment webhooks.
- **Payment providers**: BTCPay Server is required; Zaprite (card / fiat) is optional and gated by the `zaprite_payments` entitlement.
