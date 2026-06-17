# Architecture notes

## Design principles

**Decentralized by default.** Every licensing-service instance is independent. No phoning home, no shared state. If we vanish, every developer using this keeps running their own.

**Cryptography before databases.** A license key carries its own proof of legitimacy via an Ed25519 signature. The database is the authority on revocation and binding, but not on authenticity. This means downstream software doesn't break when your server has an outage.

**Idempotent webhooks.** BTCPay may retry a webhook. Settlement logic is designed so duplicate webhooks can't duplicate licenses (uniqueness enforced at the `licenses.invoice_id` column plus an existence check).

**Operator-owned secrets.** The signing key lives in SQLite and is covered by StartOS encrypted backups. The admin API key is env-driven and never logged. BTCPay credentials are env-driven. No secrets in git, no secrets in code.

## Data model

The schema lives in [`migrations/`](../migrations/) as numbered, additive
migrations (0001 through the latest — it has grown substantially past the
original five-table v0.1 schema, adding discount codes, tiered pricing,
multi-currency, subscriptions, tier upgrades, per-product entitlement catalogs,
scoped API keys, merchant profiles, and more). The core tables established in
[`0001_initial.sql`](../migrations/0001_initial.sql):

- `products` — what's for sale. Independent pricing per product.
- `invoices` — one per purchase attempt, keyed by BTCPay's invoice id.
- `licenses` — one per successful payment (or manual issuance). Has optional `fingerprint` (machine bind) and `bound_identity` (user bind) columns. Later migrations add `expires_at`, entitlements, trial flag, and tier columns.
- `validation_log` — append-only audit log of every validate call. Useful for detecting abuse (same key, many fingerprints) and for rate-limiting layers above us.
- `server_keys` — singleton table holding the server's Ed25519 keypair. Generated on first boot.

## License key format

```
LIC1 - <base32(74-byte payload)> - <base32(64-byte signature)>
```

The payload is a fixed binary layout, not JSON, to keep keys short. Details in [`src/crypto/mod.rs`](../src/crypto/mod.rs).

Why base32 Crockford-style (no padding)?

- Uppercase only, unambiguous chars, easy to read aloud or type from a screen.
- Slightly longer than base64 but less error-prone for humans copying keys.
- Case-insensitive accept means users don't get mysteriously rejected keys.

Why include `issued_at` in the signed payload?

- Lets SDKs reject keys issued before a known revocation epoch without contacting the server (future feature).
- Lets admins spot anomalies in key-age distribution when investigating abuse.

Why optional `fingerprint_hash` *inside the signature*?

- If set, the key is cryptographically useless on any other machine even if DB state is somehow lost. Belt-and-suspenders.
- Not required — most commercial licenses use trust-on-first-use via the DB column instead, because hard binding breaks legitimate hardware upgrades.

## Threat model

Who might attack this?

1. **Pirate trying to use software without paying.** Must present a valid signed key. Can't mint one without the server's private key. Can't replay a key across machines if fingerprint-bound. Can't modify a revoked key into a fresh one without breaking the signature.

2. **Someone who compromises the licensing server.** Can mint keys, revoke keys, read the DB. That's the intended failure mode — the server is the trust root. Mitigations: run on a hardened StartOS instance, use encrypted backups, don't expose admin endpoints to the clearnet (use LAN-only or Tor-only exposure in the manifest).

3. **Someone MITM-ing the /v1/validate call.** Can't forge successful responses because legitimate clients also did offline signature verification first. Can serve stale "revoked" responses — denial of service at worst, not a bypass.

4. **BTCPay webhook spoofer.** Must know the shared HMAC secret. We verify in constant time and reject bad signatures with 401.

5. **Chargeback / dispute** (applicable to non-Bitcoin rails, but worth noting). Bitcoin payments are irreversible, so the normal fraud model that motivates software DRM mostly doesn't apply here. Most revocations will be: key leaked publicly, legitimate business decision, mistaken issuance.

## What's deliberately NOT in v0.1

- **Key rotation.** A single static signing key is fine for first launch. Rotation requires SDK multi-key support and a migration strategy; deferred.
- **Trial periods / demos.** This is a pure paid-license server. Trials are the developer's responsibility in-app.
- **Payment currencies other than BTC.** BTCPay supports Lightning, altcoins, and fiat; we only send BTC-denominated invoices. Adding Lightning is straightforward (BTCPay handles it transparently if the store has LN configured).
- **Multi-tenant / SaaS mode.** This is a *single-operator* server by design. Running multiple logical operators on one instance is a different product.
- **Admin UI.** Everything is API-driven. Wrap it in whatever UI you like — or just use `curl`.

## Notes on Start9 dependencies

When you write the s9pk manifest, `btcpayserver` is a declared dependency. StartOS resolves it to a `.startos` hostname that only works on the same server. If you ever want to run licensing-service pointing at a *remote* BTCPay, you can override `BTCPAY_URL` — the client is a plain HTTPS client, not bound to the StartOS mesh.

For webhooks going the other way (BTCPay → licensing), the webhook URL BTCPay calls will be your licensing service's `.local` or `.onion` hostname. Same-server Tor hop works fine.
