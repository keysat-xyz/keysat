# Keysat Agent Integration Guide

How to build agents, bots, and automation that operate a Keysat instance.

Keysat was designed from the start to be agent-friendly. The admin API uses
plain HTTP + JSON with Bearer-token auth. There's an OpenAPI 3.1 spec for
discovery. Scoped API keys let you give an agent least-privilege access
without handing over the master credential. Errors carry stable machine-readable codes. Webhooks let an agent react to events instead of polling.

This guide is for the *operator side* of Keysat — running, configuring, and
performing day-to-day operations on a Keysat instance. For the *buyer side*
(validating licenses inside your app), see [KEYSAT_INTEGRATION.md](KEYSAT_INTEGRATION.md).

---

## Quick start

```bash
# 1. Discover the API surface
curl https://your-keysat-host/v1/openapi.json

# 2. Generate a scoped API key (in admin UI: Settings → API keys, or via curl)
curl -X POST https://your-keysat-host/v1/admin/api-keys \
  -H "Authorization: Bearer $MASTER_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"label":"Support bot","role":"support"}'
# Response includes `token: ks_...`. Save it — it's only shown once.

# 3. Use the scoped key
curl https://your-keysat-host/v1/admin/licenses?status=active \
  -H "Authorization: Bearer ks_..."
```

---

## Authentication

All admin endpoints use HTTP Bearer auth:

```
Authorization: Bearer <token>
```

Two kinds of tokens are accepted:

**Master admin API key** — the env-configured `KEYSAT_ADMIN_API_KEY` (visible
in StartOS Actions → Show credentials on first install). Full access to every
endpoint. This is the operator's credential. Don't hand it to agents.

**Scoped API keys** — additional tokens generated in admin UI → Settings →
API keys. Each carries a role that bounds what it can do. Format: `ks_<43 chars>`. Operators can revoke any scoped key from the same UI; revoked tokens stop working immediately.

### Role to scope mapping

| Role | What it can do |
|---|---|
| `read-only` | List / get every resource. Mutate nothing. |
| `license-issuer` | All `read-only` scopes + issue / revoke / suspend / change-tier on licenses. Cannot touch products, policies, or codes. |
| `support` | All `license-issuer` scopes + cancel subscriptions + force-deactivate machines. |
| `full-admin` | Every scope. Equivalent to the master key for most endpoints. |

Endpoints that touch settings (operator name, payment provider connections,
self-license activation, scoped API key management) always require the master
admin key. A `full-admin` scoped key cannot, for example, generate another
scoped key — that's a self-defeating elevation path.

---

## Discovering the API

Two complementary discovery mechanisms.

### OpenAPI 3.1 spec

`GET /v1/openapi.json` — unauthenticated. Returns a curated spec covering the
agent-relevant subset of endpoints. Use this with:

- **OpenAI Custom GPTs**: paste the URL as an Action.
- **OpenAI Assistants / Functions**: feed the spec to tool definition generators.
- **Claude tool use**: use the spec to derive your `tools` array; Claude Code agents can `WebFetch` the spec at runtime and reason about endpoints.
- **LangChain / AutoGen / Smolagents**: use their OpenAPI loaders.
- **Code generation**: `openapi-generator-cli generate -i /v1/openapi.json -g python -o ./client`.

The spec is a stable agent surface, not auto-derived from handler signatures.
We commit to keeping documented endpoints and field shapes stable across
minor releases.

### Embedded endpoint listing

This guide's "Common workflows" section below covers the most common agent
tasks with copy-paste examples.

---

## Response envelope conventions

Every error response uses the same JSON envelope:

```json
{
  "ok": false,
  "error": "tier_cap",
  "message": "Your Creator tier allows up to 5 products. You're at 5...",
  "upgrade_url": "https://licensing.keysat.xyz/buy/keysat?policy=pro"
}
```

`error` is a stable machine-readable code; `message` is human-readable. The
`upgrade_url` field appears on 402 (tier cap) responses so a UI can render an
upgrade CTA without parsing message strings.

### Error codes

| HTTP | `error` code | When |
|---|---|---|
| 400 | `bad_request` | Malformed body, missing required field, invalid enum value |
| 401 | `unauthorized` | No `Authorization: Bearer` header |
| 403 | `forbidden` | Wrong token, revoked scoped key, role doesn't grant required scope |
| 404 | `not_found` | Resource id doesn't exist |
| 409 | `conflict` | Slug collision, delete-with-references blocked, etc. |
| 402 | `tier_cap` | Operator's self-tier doesn't include the required entitlement |
| 429 | `rate_limited` | Rate limit hit (e.g. /v1/recover, /v1/validate) |
| 502 | `upstream_error` | BTCPay / Zaprite call failed |
| 503 | `service_unavailable` / `btcpay_not_configured` | Provider not yet connected |
| 500 | `internal_error` | Bug. Includes a trace id in logs; report it. |

### Validate response

`POST /v1/validate` is the one endpoint that returns 200 in all cases. Inspect
`ok` + `reason`:

| `reason` | Meaning |
|---|---|
| `bad_signature` | Signature doesn't verify against the trust-root pubkey |
| `not_found` | License key not in the daemon's DB |
| `revoked` | Operator revoked it |
| `suspended` | Operator suspended it (reversible) |
| `expired` | Past `expires_at` |
| `fingerprint_mismatch` | Different machine than the one bound on first activate |
| `product_mismatch` | License is for a different product than the caller asserted |
| `machine_cap_exceeded` | Activating this fingerprint would exceed `max_machines` |

---

## Common workflows

### Issue a comp license

```bash
curl -X POST $KS/v1/admin/licenses \
  -H "Authorization: Bearer ks_..." \
  -H "Content-Type: application/json" \
  -d '{
    "product_slug": "recap",
    "policy_slug": "pro",
    "buyer_email": "alice@example.com",
    "buyer_note": "Conference speaker comp"
  }'
```

Returns the issued license object including `license_key`. The buyer pastes
the key into their app; subsequent validate calls return `ok: true` with the
policy's entitlements.

Scope required: `licenses:write` (any role except `read-only`).

### Revoke a license

```bash
curl -X POST $KS/v1/admin/licenses/$LICENSE_ID/revoke \
  -H "Authorization: Bearer ks_..." \
  -H "Content-Type: application/json" \
  -d '{"reason":"customer request"}'
```

Idempotent. The next online validate from the buyer's app returns `reason: revoked`.

Scope required: `licenses:write`.

### Find a license by email

```bash
curl "$KS/v1/admin/licenses?buyer_email=alice@example.com" \
  -H "Authorization: Bearer ks_..."
```

Returns matching licenses (without the `license_key` field — that's only
returned on issue / recover). Use the `id` for follow-up operations.

Scope required: `licenses:read`.

### Cancel a buyer's subscription

```bash
# Look up the subscription id first (filter by license_id if you have it)
curl "$KS/v1/admin/subscriptions?status=active" \
  -H "Authorization: Bearer ks_..."

# Then cancel
curl -X POST $KS/v1/admin/subscriptions/$SUB_ID/cancel \
  -H "Authorization: Bearer ks_..." \
  -d '{"reason":"buyer requested"}'
```

License stays valid through the current cycle's `expires_at`. Renewal worker
stops issuing new invoices.

Scope required: `subscriptions:write`.

### Free a machine seat

```bash
curl -X POST $KS/v1/admin/machines/$MACHINE_ID/deactivate \
  -H "Authorization: Bearer ks_..." \
  -d '{"reason":"buyer moved devices"}'
```

The seat opens up. The buyer's next validate from any machine takes the
freed seat.

Scope required: `machines:write`.

### Programmatic tier change (comp upgrade)

```bash
curl -X POST $KS/v1/admin/licenses/$LICENSE_ID/change-tier \
  -H "Authorization: Bearer ks_..." \
  -d '{
    "target_policy_slug": "pro",
    "reason": "support resolution"
  }'
```

Always applies as comp (no invoice) from the admin path. Buyer-initiated
paid upgrades go through `/v1/upgrade` (different endpoint, signed-license auth).

Scope required: `licenses:write`.

---

## Webhooks — react to events instead of polling

Configure webhook endpoints in admin UI → Webhooks. The daemon POSTs JSON
payloads, HMAC-SHA256 signed with the endpoint's secret, on these events:

| Event | Fires on |
|---|---|
| `license.issued` | New license minted (purchase, comp, redeem) |
| `license.revoked` / `license.suspended` / `license.unsuspended` | Admin operations |
| `license.tier_changed` | Tier upgrade/downgrade applied |
| `invoice.paid` | A BTCPay/Zaprite invoice settled |
| `subscription.renewal_pending` | Renewal worker created a fresh invoice |
| `subscription.renewal_skipped` | Renewal skipped (e.g. policy archived) |
| `subscription.cancelled` | Buyer or admin cancelled |
| `subscription.lapsed` | Past_due grace expired |
| `machine.activated` | First validate from a new fingerprint |

Verify signatures:

```python
import hmac, hashlib

def verify(body_bytes: bytes, signature_header: str, secret: str) -> bool:
    expected = hmac.new(secret.encode(), body_bytes, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature_header)
```

The header is `X-Keysat-Signature`. Failed deliveries retry with exponential
backoff up to 10 attempts; permanently-failed deliveries land in the DLQ
visible at admin UI → Webhooks → Failed.

---

## Designing a robust agent

A few patterns that work well in practice.

### Idempotency

The daemon's mutation endpoints are idempotent where they can be. Revoke,
suspend, unsuspend, archive, unarchive, subscription cancel — all return
success on the second call without changing state. Your agent can safely
retry on network errors.

### Pagination

List endpoints return up to ~100 rows by default. Use `?limit=N` and
`?offset=N` for larger result sets. The OpenAPI spec documents the limits
per endpoint.

### Rate limits

The admin endpoints have no per-IP rate limit today — operators are trusted.
The public endpoints (`/v1/validate`, `/v1/recover`) are rate-limited per
client IP (10/min for /recover; /validate is unlimited but a reasonable
agent calls it once per app boot + once per hour).

### Master key handling

If your automation needs `full-admin` because it touches operator-only
operations (creating other API keys, changing payment providers), use the
master key from a secret manager. If it can stay within license / product /
policy operations, **always use a scoped key**. Operators can revoke a
compromised scoped key without rotating the master credential.

### Backoff on 5xx

`internal_error` (500) is a bug or a transient DB lock. Retry with exponential
backoff (1s, 2s, 4s, 8s, give up). Don't retry on 4xx — those are deterministic
client errors.

---

## Concrete agent recipe — "Comp a license to anyone who emails support@"

```python
import os, requests, imaplib, email

KS = os.environ["KEYSAT_URL"]
TOKEN = os.environ["KEYSAT_API_KEY"]  # license-issuer-scoped key

def issue_comp_license(buyer_email: str, product_slug: str, reason: str) -> str:
    r = requests.post(
        f"{KS}/v1/admin/licenses",
        headers={"Authorization": f"Bearer {TOKEN}"},
        json={
            "product_slug": product_slug,
            "policy_slug": "default",
            "buyer_email": buyer_email,
            "buyer_note": reason,
        },
        timeout=10,
    )
    r.raise_for_status()
    return r.json()["license_key"]

# Poll IMAP, parse incoming requests, call issue_comp_license, reply with the key
```

That's the entire pattern. The agent doesn't need full admin — just the
license-issuer role. If it ever gets compromised, you revoke the scoped key
in the admin UI and generate a new one in 30 seconds.

---

## What's NOT exposed to agents

Some operations are deliberately operator-only and not accessible to any
scoped key, including `full-admin`:

- Generating / revoking scoped API keys (`/v1/admin/api-keys`)
- Connecting / disconnecting payment providers
- Setting the operator name
- Activating the self-license (`/v1/admin/self-license`)
- Resetting the analytics install_uuid
- Changing the web UI password (StartOS Action only)

These all require the master `KEYSAT_ADMIN_API_KEY`. The reasoning: an agent
that can rotate its own credentials, connect arbitrary payment processors, or
change the operator identity is no longer bounded by the role it was given.

---

## Help us improve this guide

The OpenAPI spec is the source of truth for the API surface. This guide is a
hand-curated overlay focused on the workflows we've seen agents actually need.
If you're building something the spec covers but this guide doesn't make
obvious, open an issue at github.com/keysat-xyz/keysat with the workflow
shape and we'll add it.
