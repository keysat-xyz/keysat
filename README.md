# keysat-startos

StartOS 0.4.0.x wrapper package for [Keysat](../licensing-service) (the Rust daemon in `../licensing-service/`). This directory turns the upstream Rust daemon into an installable `.s9pk`.

The source directory is still called `licensing-service/` on disk for continuity; the binary it produces, the manifest id, and all operator-visible strings use the new name **Keysat**.

## Prerequisites

- A working StartOS 0.4.0.x development environment (see [docs.start9.com](https://docs.start9.com)).
- `start-cli` installed, with `~/.startos/developer.key.pem` initialized.
- Node.js and npm (the StartOS SDK is TypeScript).
- Docker (via buildx) for the multi-arch image build.

## Getting the shared build logic

The `Makefile` includes `s9pk.mk`, which is shared build boilerplate maintained by the Start9 team. Fetch it once:

```bash
curl -o s9pk.mk https://raw.githubusercontent.com/Start9Labs/hello-world-startos/master/s9pk.mk
```

(Or copy it from any other 0.4.0.x package you have locally.)

## Installing dependencies

```bash
npm install
```

## Building and installing

```bash
# Build for all supported architectures
make

# Or just the architecture of your dev StartOS box
make arm        # for Raspberry Pi / Apple Silicon StartOS
make x86        # for an x86 StartOS server

# Push to your StartOS server (requires the developer key)
make install
```

`make install` will prompt for your StartOS password the first time; subsequent installs use the cached session.

## Project layout

This follows the standard 0.4.0.x layout:

```
keysat-startos/
├── Dockerfile                         # multi-stage Rust build
├── Makefile                           # delegates to s9pk.mk
├── s9pk.mk                            # (fetch from hello-world-startos)
├── package.json / tsconfig.json
├── icon.png                           # 512×512 StartOS tile
├── assets/
│   ├── ABOUT.md
│   └── keysat-thumbnail.png           # 1024×1024 marketing hero
└── startos/
    ├── manifest/index.ts              # setupManifest()
    ├── manifest/i18n.ts               # descriptions, translatable
    ├── main.ts                        # daemon definition
    ├── interfaces.ts                  # network exposure (API on 8080)
    ├── dependencies.ts                # requires BTCPay Server
    ├── actions/                       # user-facing StartOS buttons
    │   ├── configureBtcpay.ts     # one-click BTCPay authorize
    │   ├── createPolicy.ts        # reusable license template
    │   ├── createProduct.ts
    │   ├── deactivateMachine.ts   # force-kick an install
    │   ├── issueLicense.ts        # comp / press keys
    │   ├── listMachines.ts        # inspect a license's seats
    │   ├── listWebhooks.ts
    │   ├── registerWebhook.ts     # outbound event subscriber
    │   ├── revokeLicense.ts       # one-way permanent block
    │   ├── searchLicenses.ts      # lost-key recovery
    │   ├── setOperatorName.ts
    │   ├── showCredentials.ts
    │   ├── suspendLicense.ts      # reversible lockout
    │   ├── unsuspendLicense.ts
    │   └── viewAuditLog.ts
    ├── fileModels/store.ts            # persistent wrapper state
    ├── init/index.ts                  # first-boot setup
    ├── versions/                      # migration history
    │   ├── index.ts
    │   └── v0.1.0.ts
    ├── backups.ts                     # volume backup declaration
    ├── sdk.ts                         # manifest-bound SDK instance
    ├── utils.ts                       # small helpers
    └── index.ts                       # ties everything together
```

## Dockerfile notes

The Dockerfile expects the `licensing-service/` source to be available at the parent directory (`..`). The manifest sets `images.main.source.dockerBuild.workdir` to `'..'` so `start-cli s9pk pack` runs `docker build` with the parent `Licensing/` directory as the context — Docker then sees the licensing-service source alongside this wrapper. A `.dockerignore` at the parent level keeps the uploaded context small.

If you're laying out the repositories differently — e.g., separate GitHub repos for service and wrapper — you'll want to add a git submodule or adjust the `workdir`/`COPY` paths accordingly.

## Operator workflow after install

1. Open the service in your StartOS dashboard.
2. **Set operator name** → your display name, shown on the public homepage.
3. **Connect BTCPay** → one-click authorize flow. Opens BTCPay's consent page in your browser; after you approve, the daemon auto-detects your store and registers its inbound webhook. No API keys to copy.
4. **Check BTCPay connection** to confirm the authorize succeeded.
5. **Create product** once per thing you want to sell.
6. **Create policy** at least once per product, slugged `default`, to set the shape of keys issued through the public purchase flow (duration, grace period, entitlements, seat cap).
7. Share the public service URL with buyers. That's enough for the standard purchase flow.

### Customer support

- **Search licenses** — look up a buyer by email, Nostr npub, or BTCPay invoice id.
- **Suspend license** / **Unsuspend license** — reversible lockout (e.g., for payment disputes).
- **Revoke license** — permanent, one-way kill.
- **Issue license manually** — comp / press / grandfathered keys.
- **List machines** — see which installs are bound to a license.
- **Deactivate machine** — force-kick a specific install, freeing a seat.

### Integrations & operations

- **Register webhook endpoint** — POST signed event notifications to an HTTPS URL you control (license.issued, license.revoked, machine.activated, etc.). HMAC-SHA256 in `X-Keysat-Signature: sha256=<hex>`.
- **List webhook endpoints** — see what's subscribed.
- **View audit log** — most recent admin mutations, filterable by action slug. Useful for compliance and debugging.
- **Show admin API key** — only needed if you want to script against `/v1/admin/*` from outside the box; every built-in action already carries the key for you.

## Limitations in v0.1

- No in-dashboard list view for invoices/products/licenses — use `/v1/admin/...` via the admin API key if you need a bulk view beyond what the built-in actions surface.
- Webhook delivery retries are bounded; if a subscriber is down past the retry window, the event is dropped. Invoice reconciliation runs as a background task so dropped BTCPay webhooks get replayed.
