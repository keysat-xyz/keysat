# About Keysat

**Keysat** is a self-hosted, Bitcoin-paid software licensing server: every operator runs their own instance on their own hardware, so there is no central authority, no shared database, and no lock-in. You own the signing key, the customer records, and the payment rails.

After installing:

1. Click **Connect BTCPay** once to authorize the daemon against your BTCPay Server (one-click — nothing to copy and paste).
2. Click **Create product** for each thing you want to sell.
3. Optionally click **Create policy** to set per-product defaults (duration, grace period, entitlements, seat cap, trial flag) — a policy slugged `default` is used by the public purchase flow.
4. Share your Keysat URL with buyers. They call `POST /v1/purchase`, pay via BTCPay, and Keysat issues an Ed25519-signed license key your software can verify offline.

The same in-dashboard action buttons cover license issuance (for comps, press, trials), suspension / unsuspension, revocation, machine management, outbound webhook subscriptions, and an audit log viewer. Full developer docs live in the upstream repository.
