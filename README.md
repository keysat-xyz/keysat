<p align="center">
  <img src="icon.png" alt="Keysat" width="128" />
</p>

<h1 align="center">Keysat</h1>

<p align="center">
  Self-hosted licensing server. Sell software on payment channels you control,
  verify licenses offline, keep the keys + customer list on your hardware. Runs on Start9.
</p>

<p align="center">
  <a href="https://keysat.xyz">keysat.xyz</a> &middot;
  <a href="https://docs.keysat.xyz">docs.keysat.xyz</a> &middot;
  <a href="https://github.com/keysat-xyz/keysat/releases">Releases</a>
</p>

---

## Quick start

**Operator (install Keysat on your Start9):** add `registry.keysat.xyz` to your StartOS marketplace and install. Sideload the `.s9pk` from [GitHub releases](https://github.com/keysat-xyz/keysat/releases/latest) if you prefer. See [Install &amp; setup](https://docs.keysat.xyz/install.html) for the full walkthrough.

**Developer (verify a license in your software):** four official SDKs ship today, all wire-compatible against the same cross-check fixtures in [`licensing-service/tests/crosscheck/`](licensing-service/tests/crosscheck/).

| Language | Install |
|---|---|
| TypeScript | `npm install @keysat/licensing-client` |
| Rust | `cargo add keysat-licensing-client` |
| Python | `pip install keysat-licensing-client` |
| Go | `go get github.com/keysat-xyz/keysat-client-go` |

See [Integrate the SDK](https://docs.keysat.xyz/integrate.html) for the five-line verifier pattern.

**Operator agent / automation:** the daemon exposes an OpenAPI 3.1 spec, scoped API keys with role-based access, and outbound webhooks. See [Agent integration](https://docs.keysat.xyz/agent.html).

---

> **About this README.** Keysat is a from-scratch service authored for
> StartOS — there is no upstream project to differ from. The canonical
> implementation is this package and the Rust daemon it wraps
> (`licensing-service/`). Where this README would normally explain
> "differences from upstream," it instead documents the architecture
> directly. Anything that isn't documented here matches the source.

## Table of Contents

- [What Keysat is](#what-keysat-is)
- [Image and Container Runtime](#image-and-container-runtime)
- [Volume and Data Layout](#volume-and-data-layout)
- [Installation and First-Run Flow](#installation-and-first-run-flow)
- [Configuration Management](#configuration-management)
- [Network Access and Interfaces](#network-access-and-interfaces)
- [Actions (StartOS UI)](#actions-startos-ui)
- [Backups and Restore](#backups-and-restore)
- [Health Checks](#health-checks)
- [Dependencies](#dependencies)
- [Limitations and Differences](#limitations-and-differences)
- [What Is Unchanged from Upstream](#what-is-unchanged-from-upstream)
- [Contributing](#contributing)
- [YAML Quick Reference](#yaml-quick-reference)

## What Keysat is

Keysat lets a software seller issue, validate, and revoke license keys for
their own product, with payment in Bitcoin via BTCPay Server. The seller
runs Keysat on their own Start9, declares one or more products, and shares
a public purchase URL with their customers. Buyers pay in Bitcoin and
receive a signed license key whose authenticity their software can verify
offline against the seller's embedded public key. Keys can be capped to
specific machines, time-limited, suspended, revoked, or marked as trial.

Discount and referral codes (paid and free-license) are first-class
primitives. Free-license codes bypass BTCPay entirely and issue a key
directly via a public redemption endpoint — useful for press passes,
comp keys, beta access, or "first N users free" launch promos.

## Image and Container Runtime

Built from the local `Dockerfile` via `images.main.source.dockerBuild`,
with build context set to the parent directory so the Dockerfile can
`COPY` from the sibling `licensing-service/` source tree. The Rust binary
is statically linked against musl (target
`*-unknown-linux-musl`) so the runtime image is a `scratch`-based final
stage with no shared-library dependencies. Architectures: `x86_64` and
`aarch64`.

`start-cli s9pk pack` ingests the resulting OCI image, converts it to a
squashfs filesystem image, and embeds that in the `.s9pk`. At runtime
StartOS extracts the squashfs and runs the service in its own container
runtime.

## Volume and Data Layout

Keysat declares a single persistent volume:

| Volume | Mount  | Contents                                                |
|--------|--------|---------------------------------------------------------|
| `main` | `/data`| SQLite database (`keysat.db`); contains the Ed25519 signing keypair, products, policies, licenses, machines, invoices, redemptions, audit log, and BTCPay credentials. |

Loss of this volume invalidates every issued license, since the signing
keypair is regenerated on first boot. Treat StartOS-managed backups as
mandatory.

## Installation and First-Run Flow

1. Install Keysat via the marketplace (or sideload the `.s9pk`).
2. Resolve the auto-created **critical task** "Connect BTCPay" by
   running the **Connect BTCPay** action. This opens a one-click
   authorize page on your local BTCPay; after approval, Keysat
   auto-detects your store and registers an inbound webhook. No API
   keys to copy.
3. Run **Check BTCPay connection** to confirm — the install task clears
   automatically.
4. Set your **operator name** (shown on the public homepage and in
   buyer-facing receipts).
5. Create one or more **products** — each represents something you sell.
6. Create at least one **policy** per product. Multi-tier ladders
   (Basic / Pro / Max) are first-class: when a product has two or more
   public policies, the buy page renders a tier picker and the buyer
   chooses before paying. Policies define duration, grace period, seat
   cap, entitlements, recurring cadence, trial flag, price overrides,
   marketing bullets, and per-entitlement hide-on-buy-page toggles.
7. Optionally create **discount / referral / free-license codes** (see
   `Create discount code` action).
8. Share the public service URL with buyers.

## Configuration Management

All configuration is performed through StartOS actions; there is no
on-disk config file the operator should edit. Environment variables
passed to the daemon at startup (`main.ts`) are derived from the
package-local store (operator name, admin API key) and from the
declared BTCPay dependency hostname.

For advanced operators, the `/v1/admin/*` HTTP API exposes everything
the actions do plus bulk-list operations not yet surfaced in the UI.
Retrieve the admin API key via the **Show admin credentials** action.

## Network Access and Interfaces

Keysat exposes one logical port (8080 HTTP) split across two service
interfaces for clarity:

| Interface | Type | Path prefix | Purpose                                                                      |
|-----------|------|-------------|------------------------------------------------------------------------------|
| `api`     | api  | `/`         | Public REST API for buyers (purchase, redeem) and licensed apps (validate, machine activation). Bake the URL into your software builds as the licensing endpoint. |
| `webhook` | api  | `/btcpay`   | BTCPay webhook landing endpoint. Registered automatically during Connect BTCPay; not for human use. |

StartOS terminates TLS at the platform edge. Inside the container every
request arrives as plain HTTP. For browser-facing URLs (e.g., the public
purchase page) hardcode `https://`.

## Actions (StartOS UI)

Grouped as displayed in the dashboard.

**General**
- *Set operator name* — your public-facing brand.

**BTCPay**
- *Connect BTCPay* — one-click authorize against your BTCPay; auto-detects store and registers webhook.
- *Check BTCPay connection* — confirm BTCPay state; clears the install task on success.

**Credentials**
- *Show admin credentials* — admin API key for direct `/v1/admin/*` access.

**Products + Policies**
- *Create product* — declare something to sell.
- *Create policy* — license template for a product (duration, grace, seat cap, entitlements, trial flag, price override).

**Discount codes**
- *Create discount code* — percent-off / fixed-sats-off / free-license.
- *List discount codes* — usage stats.
- *Disable / enable discount code*.

**Licenses**
- *Issue license manually* — comp / press / grandfathered keys.
- *Search licenses* — by email or BTCPay invoice id.
- *Suspend license* — reversible lockout.
- *Unsuspend license*.
- *Revoke license* — terminal kill.

**Machines**
- *List machines* — installs bound to a license.
- *Deactivate machine* — free a seat.

**Webhooks (outbound)**
- *Register webhook endpoint* — POST signed events to your URL.
- *List webhook endpoints*.

**Diagnostics**
- *View audit log* — admin mutation history, filterable.

## Backups and Restore

Keysat opts into StartOS's default volume backup via `setupBackups` /
`Backups.ofVolumes('main')`. The single `main` volume contains all
state — signing key included — so a backup is sufficient to fully
recover the service. On restore, the install-time **Connect BTCPay**
task re-surfaces in case the BTCPay credentials in the restored DB are
stale.

Treat backups as mandatory: losing the signing keypair invalidates every
key Keysat ever issued, with no recovery path.

## Health Checks

A single port-listening check on port 8080 (`sdk.healthCheck.checkPortListening`).
StartOS reports the service as healthy once the daemon is binding the
port. The daemon exposes `GET /healthz` for richer external monitoring.

## Dependencies

| Dependency  | Version range | Required | Purpose                                                       |
|-------------|---------------|----------|---------------------------------------------------------------|
| `btcpayserver` | `>=1.11.0` | Yes      | Required to receive Bitcoin payments and confirm settlement.  |

The dependency is `kind: 'running'`, so Keysat will not start until
BTCPay is running. The `btcpayserver.startos` hostname is provided to
the container automatically.

## Limitations and Differences

Known current limitations:

- **Buyer self-service recovery is by-design minimal.** Buyers can re-derive a lost license at `/recover` using (invoice id, buyer email). They cannot transfer between machines without contacting the operator (use *Free a machine seat* in the admin / agent API).
- **No bulk / volume licensing UI.** "Buy 10 keys at once with discount" is not built into the buy page. Operators can issue N comp licenses via the admin API in a loop.
- **Webhook delivery retries are bounded.** A subscriber down past the 10-attempt retry window lands in the dead-letter queue (visible in admin UI → Webhooks → Failed). BTCPay invoice reconciliation runs as a background poll so dropped *payment* webhooks are recovered.
- **Hardware fingerprinting is client-supplied.** Keysat does not derive fingerprints itself; the buyer-side SDK passes whatever the integrator chose. The fingerprint is bound on first activate and enforced thereafter.
- **Card payments not shipped.** The Zaprite payment provider is in design for v0.3 — operators on Pro / Patron will get a card-payment option alongside BTCPay. Until then, payments are BTC / Lightning only.

## What Is Unchanged from Upstream

Not applicable — Keysat is authored fresh for Start9 and has no upstream.
The canonical implementation IS this package + the Rust daemon at
`licensing-service/`.

## Contributing

For commercial redistribution or resale rights, or to discuss white-label
deployment, contact `licensing@keysat.xyz`. Source-available license
terms are in the package's `LICENSE` file: you may run, audit, modify
for self-hosting; you may not redistribute, resell, or publicly host for
others.

## YAML Quick Reference

Structured summary for AI consumers and automated package introspection.

```yaml
service:
  id: keysat
  title: Keysat
  category: bitcoin
  license: source-available (LicenseRef-Proprietary)
  marketingUrl: https://keysat.xyz
image:
  source: dockerBuild
  baseImage: scratch (musl-static Rust binary)
  arches: [x86_64, aarch64]
volumes:
  - id: main
    mountpoint: /data
    contents: SQLite DB + Ed25519 signing keypair
network:
  interfaces:
    - id: api
      type: api
      port: 8080
      protocol: http
      pathPrefix: /
      audience: public
    - id: webhook
      type: api
      port: 8080
      protocol: http
      pathPrefix: /btcpay
      audience: btcpay
dependencies:
  btcpayserver:
    required: true
    versionRange: ">=1.11.0"
    kind: running
healthChecks:
  - id: api
    method: portListening
    port: 8080
backups:
  mode: full-volume
  volumes: [main]
firstRun:
  tasks:
    - id: btcpay-initial-setup
      severity: critical
      runs: configureBtcpay
features:
  paymentRail: btcpay-server   # zaprite planned for v0.3 (card payments)
  signing: ed25519
  offlineVerification: true
  multiSeat: true
  trialFlag: true
  expiry: true
  gracePeriod: true
  entitlements: true
  entitlementsCatalog: per-product   # typed slugs with display names + descriptions
  hiddenEntitlements: per-policy    # license-granted but hidden from buy page
  marketingBullets: per-policy      # operator-authored ✓ items on tier cards
  multiCurrency: [SAT, USD, EUR]    # auto-converted at invoice creation
  discountCodes: [percent, fixed_sats, set_price, free_license]
  featuredDiscounts: true   # launch-special, auto-applies on the buy page
  multiPolicyDiscountScope: true   # one code can apply to N policies
  recurringSubscriptions: true   # auto-renew with trials + grace
  tierUpgrades: true   # in-place tier upgrade with proration
  outboundWebhooks: true
  webhookDlq: true   # failed deliveries retryable from admin UI
  auditLog: true
  scopedApiKeys: [read-only, license-issuer, support, full-admin]
  openapiSpec: /v1/openapi.json
  selfLicensingTier: [Creator, Pro, Patron]
sdks:
  - typescript: "@keysat/licensing-client (npm)"
  - rust: "keysat-licensing-client (crates.io)"
  - python: "keysat-licensing-client (PyPI)"
  - go: "github.com/keysat-xyz/keysat-client-go"
```
