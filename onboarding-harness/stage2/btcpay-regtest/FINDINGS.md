# De-risk result — BTCPay regtest network detection (agent-payment-connect slice 3)

**Verdict: the spec's primary network-detection assumption (§6.1) is VALIDATED against
a live regtest BTCPay 2.x. No blocker; slice 3 needs no extra OAuth permission.**

Rig: `docker-compose.yml` in this dir — bitcoind(regtest) + NBXplorer + postgres +
btcpayserver `2.0.6`. Validated 2026-06-16. Probe: `probe.sh`; raw payloads in
`probe-out/`. Bring up `docker compose -p keysat-btcpay up -d`; tear down
`docker compose -p keysat-btcpay down -v`.

## What the gate will actually see

1. **Payment-method id is `BTC-CHAIN`** on BTCPay 2.x. Posting to the legacy `.../BTC/...`
   path is normalized to `BTC-CHAIN`. **Do not hardcode** — BTCPay 1.x used `BTC`. Slice 3
   should read `paymentMethodId` from the list and pick the on-chain BTC method
   (id ∈ {`BTC-CHAIN`,`BTC`}, not Lightning).

2. **Primary signal — receive address HRP (spec §6.1 primary), CONFIRMED:**
   `GET /api/v1/stores/{id}/payment-methods/BTC-CHAIN/wallet/address`
   → `{"address":"bcrt1qwsh9ua5qeutshvrhz474uduwqlw8gfukfpc8vt","keyPath":"0/0","paymentLink":...}`
   `bcrt1…` HRP ⇒ **regtest** ⇒ non-mainnet ⇒ scoped connect allowed (on a sandbox daemon).
   Classification table (validated regtest arm; others by HRP spec):
   `bc1`/base58 `1`,`3` → mainnet (deny scoped) · `tb1` → testnet/signet · `bcrt1` → regtest ·
   base58 `m`,`n`,`2` → test/regtest.

3. **Secondary signal — derivation, CONFIRMED but field name differs from the spec.**
   The spec says `derivationScheme`; on BTCPay 2.x Greenfield it is
   **`config.accountDerivation`** (and `config.signingKey`, `config.accountKeySettings[].accountKey`),
   value `tpubDC…` for regtest/testnet (mainnet → `xpub/ypub/zpub`). The BIP-84 account path
   is `84'/1'/0'` — coin-type `1'` is itself a testnet/regtest marker. **Requires
   `?includeConfig=true`** — see permission note below.

## Permission — the daemon already has enough

- The daemon's BTCPay OAuth (`REQUESTED_PERMISSIONS`, `btcpay_authorize.rs:45`) already
  requests **`btcpay.store.canmodifystoresettings`** (for webhook registration).
- Empirically, with a token holding only `canmodifystoresettings`:
  `wallet/address` → **HTTP 200**, and `payment-methods?includeConfig=true` → config **visible**.
- `wallet/address` specifically needs `canmodifystoresettings` (`canviewstoresettings` →
  **403**). The `config`/derivation path needs only `canviewstoresettings`.
- ⇒ **Slice 3 can use EITHER signal with the key it already obtains at connect. No new
  OAuth scope.** Recommend the **address-HRP path** (spec's primary; one call; unambiguous).

## Fail-closed cases (all confirmed → treat as mainnet → master-only)

- No on-chain wallet configured → `GET payment-methods` returns `[]` (no BTC-CHAIN method).
- `wallet/address` on a store with no wallet → **HTTP 503** `"BTC-CHAIN services are not
  currently available"`. (Same 503 also appears transiently while BTCPay is not yet
  `synchronized:true` — at operator connect time it will be synced, but treat any non-2xx /
  missing address / unrecognized HRP as "cannot determine" ⇒ deny scoped, require master.)

## Implication for the daemon client (slice 3)

The existing `btcpay/client.rs::list_payment_methods` calls `GET .../payment-methods`
**without** `includeConfig`, so today it sees `config:null` (confirmed). To detect network,
add a small client fn that GETs `.../payment-methods/{pmid}/wallet/address` and classifies
the HRP (preferred), or pass `?includeConfig=true` and read `config.accountDerivation`.
Resolve target network **before persisting** the provider (spec §7).

## Rig gotcha (for whoever rebuilds this)

NBXplorer defaults to cookie auth; with separate datadir volumes BTCPay can't read the
cookie → `401` → BTCPay never reaches `synchronized:true` → on-chain `BTC-CHAIN` service
stays unavailable (`503`). Fix used here: `NBXPLORER_NOAUTH=1` (fine for a throwaway
regtest box). A production-faithful harness would instead share NBXplorer's datadir volume
into BTCPay so the cookie is shared.
