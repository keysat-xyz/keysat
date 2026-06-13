# Keysat Licensing — Instructions

Keysat is a Bitcoin-native, self-hosted licensing service for software
creators. You run your own instance, hold your own signing key, and issue
Ed25519-signed license keys that your software verifies offline. There is no
central authority and no shared database.

## Before you start

- **BTCPay Server is required.** Install and start BTCPay Server first — Keysat
  uses it to take Bitcoin/Lightning payments and confirm settlement. StartOS
  lists this dependency before it lets you install Keysat.
- **A clearnet domain is recommended if you sell to the public**, so buyers
  anywhere can reach your checkout. LAN/Tor-only works for testing.
- **Zaprite is optional** (adds card payments). You connect it later from inside
  the admin web UI; nothing to do up front.

## First-time setup

1. **Get your admin API key.** Open the **Actions** tab and run
   **Show admin API key**. Copy it — you sign into the admin web UI with it the
   first time.
2. **Open the admin dashboard.** Click **Launch UI** on the **Admin Web UI**
   interface and paste the admin API key to sign in.
3. **(Recommended) Set a real password.** Run the **Set web UI password** action
   (Actions tab, minimum 12 characters). After this the login page shows a
   password field; the admin API key keeps working for automation.
4. **Connect your payment provider.** In the admin web UI's Settings, use the
   one-click **Connect BTCPay** flow to authorize Keysat against your BTCPay
   Server. (Optionally connect Zaprite here too.)
5. **Set your operator name** in the admin web UI — it appears on buyer-facing
   checkout and receipts.
6. **Create what you sell.** Use **Create product** for each item, and
   optionally **Create policy** to set per-product defaults (duration, grace
   period, entitlements, seat cap, trial flag). A policy slugged `default` is the
   one the public purchase flow uses.

Activation is optional. Keysat runs out of the box at the free **Creator** tier
(up to 5 products, 5 policies per product, and 10 active discount codes).
Activating a license lifts those caps and unlocks recurring billing and Zaprite
(card) payments. To activate, get a key at
[registry.keysat.xyz](https://registry.keysat.xyz), run the **Activate Keysat
license** action, and confirm with **Show Keysat license status**.

## Selling licenses

Share your **Licensing API** URL with buyers and bake it into your software as
the validation endpoint. Buyers call `POST /v1/purchase`, pay via BTCPay, and
Keysat issues a signed license key. Your software validates keys against
`POST /v1/validate` — including revocation checks, which return
`ok: false` with `reason: "revoked"`.

The same admin web UI covers manual license issuance (comps, press, trials),
suspension/unsuspension, revocation, machine management, discount codes,
outbound webhooks, and the audit log.

## Interfaces and exposure

- **Licensing API** (`/`) — public-facing. This is the URL you share with
  customers and bake into your builds.
- **Admin Web UI** (`/admin`) — your dashboard. Restrict this interface to LAN or
  Tor only; the public internet does not need to reach it.
- **BTCPay webhook endpoint** (`/btcpay`) — registered with BTCPay automatically
  during the Connect BTCPay flow. Not for human use.

## Backups and uninstalling

Your data volume holds the SQLite database — which contains your server signing
key and every license record — and StartOS backs it up automatically. Your
self-license at `/data/keysat-license.txt` is included in the backup and
survives upgrades and reinstalls.

**Uninstalling deletes your signing key and all license records.** Once it is
gone, previously issued license keys no longer validate against this server. Back
up first if you plan to reinstall.

## Recovery

- **Locked out of the admin UI?** Run **Set web UI password** to set a new one,
  or **Show admin API key** to sign in with the key.
- **Lost your Keysat license?** Re-run **Activate Keysat license** with your key.

## More

Full developer and integration documentation lives in the upstream repository
(`README.md` and `KEYSAT_INTEGRATION.md`) and at
[keysat.xyz](https://keysat.xyz).
