# API reference

All endpoints are JSON in / JSON out. Errors return a body of the form:

```json
{ "ok": false, "error": "not_found", "message": "product 'xyz'" }
```

Admin endpoints require `Authorization: Bearer $LICENSING_ADMIN_API_KEY`.

---

## Public endpoints

### `GET /`

Service metadata including the Ed25519 public key. Useful for SDKs to fetch the key at build time.

```json
{
  "service": "keysat",
  "version": "0.1.0",
  "operator": "Acme Software",
  "public_key_pem": "-----BEGIN PUBLIC KEY-----\nMCow...\n-----END PUBLIC KEY-----\n",
  "key_algorithm": "ed25519",
  "key_format_version": 1
}
```

### `GET /healthz`

Liveness probe. Returns `{"ok": true}`.

### `GET /v1/pubkey`

Just the public key.

### `GET /v1/products`

List all active products.

### `GET /v1/products/:slug`

Single product by slug.

### `POST /v1/purchase`

Start a purchase.

Request:

```json
{
  "product": "my-app",
  "buyer_email": "alice@example.com",
  "buyer_note": "optional",
  "redirect_url": "https://myapp.example.com/thanks"
}
```

Response:

```json
{
  "invoice_id": "uuid-of-our-row",
  "btcpay_invoice_id": "...",
  "checkout_url": "https://btcpay.example.com/i/...",
  "amount_sats": 50000,
  "poll_url": "https://license.example.com/v1/purchase/uuid-of-our-row"
}
```

### `GET /v1/purchase/:invoice_id`

Poll for license delivery.

While pending:

```json
{
  "invoice_id": "...",
  "status": "pending",
  "product_id": "...",
  "amount_sats": 50000,
  "license_key": null,
  "license_id": null
}
```

Once settled:

```json
{
  "invoice_id": "...",
  "status": "settled",
  "product_id": "...",
  "amount_sats": 50000,
  "license_key": "LIC1-...-...",
  "license_id": "..."
}
```

### `POST /v1/validate`

The hot path. Downstream software calls this at startup (and on a cadence) to check revocation.

Request:

```json
{
  "key": "LIC1-...-...",
  "product_slug": "my-app",
  "fingerprint": "sha256-of-some-installation-unique-data"
}
```

`product_slug` and `fingerprint` are optional. If `fingerprint` is provided and the license row has no fingerprint bound yet, the first caller's fingerprint is locked to the license (trust-on-first-use). Later callers presenting a different fingerprint are rejected with `reason: "fingerprint_mismatch"`.

Response (always HTTP 200 so middleware doesn't log these as errors):

```json
{ "ok": true, "license_id": "...", "product_id": "...", "product_slug": "my-app", "issued_at": "..." }
```

On failure:

```json
{ "ok": false, "reason": "revoked" }
```

Possible `reason` values: `bad_format`, `bad_signature`, `not_found`, `revoked`, `product_mismatch`, `fingerprint_mismatch`.

### `POST /v1/btcpay/webhook`

Landing point for BTCPay Server webhook events. Only BTCPay should call this. We verify `BTCPay-Sig` HMAC before trusting anything.

---

## Admin endpoints

All of these require `Authorization: Bearer $LICENSING_ADMIN_API_KEY`.

### `POST /v1/admin/products`

```json
{
  "slug": "my-app",
  "name": "My App",
  "description": "...",
  "price_sats": 50000,
  "metadata": { "anything": "useful" }
}
```

### `PATCH /v1/admin/products/:id/active`

Activate or deactivate a product.

```json
{ "active": false }
```

Deactivated products are hidden from public listings and reject new purchases; existing licenses continue to validate.

### `GET /v1/admin/licenses?product_id=...`

List licenses for a product.

### `POST /v1/admin/licenses`

Manually issue a license outside the purchase flow — for comps, press keys, developer testing.

```json
{ "product_slug": "my-app", "note": "comp for @alice" }
```

Response:

```json
{
  "license_id": "...",
  "product_id": "...",
  "license_key": "LIC1-...-...",
  "issued_at": "..."
}
```

### `POST /v1/admin/licenses/:id/revoke`

```json
{ "reason": "chargeback" }
```

Idempotent: revoking an already-revoked license returns 404.
