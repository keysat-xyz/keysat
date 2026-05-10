# Integrating Keysat licensing into your software

This document is the complete instruction set for adding Keysat-based
licensing to any application. It covers Node/TypeScript, Python, and Rust.
Hand it to an LLM (or a developer) along with your codebase and ask them
to wire it up — they should have everything they need.

## How to use this document

Drop `KEYSAT_INTEGRATION.md` into a fresh session against the target
codebase, then prompt:

> Read this `KEYSAT_INTEGRATION.md`. Then add Keysat licensing to this
> codebase. Ask the software creator how they would like to license,
> monetize and/or gate their software, including how unlicensed software
> should operate. Use the creator's Keysat public key which can be found
> in the Keysat admin UI. After wiring, summarize what you changed.

> **Reading this as an LLM?** Your job is to add license-key reading,
> signature verification, and entitlement-based feature gating to the
> target application. Before you write any code, ask the software creator
> the questions in section 0. Then read sections 1–6, then jump to the
> language section (7a/7b/7c) that matches your target codebase. The
> "Worked example" in section 14 is the canonical pattern to mimic.

---

## 0. Questions to ask the software creator before writing code

Don't write any code until you have answers. The whole licensing model
hangs on these:

1. **What's the operator's Keysat instance URL?**
   (e.g. `https://licensing.example.com`. Used for online validation
   and the in-app purchase flow.)
2. **What's the operator's product slug?**
   (Short string the operator chose when creating the product in their
   Keysat admin. License keys are scoped to this slug.)
3. **What's the operator's signing public key?**
   (PEM-formatted Ed25519 public key. Get it from the Keysat admin
   Overview tab → "Embed your public key" → Copy. The operator pastes
   it; you embed it.)
4. **How should unlicensed users experience the app?** Three legitimate
   patterns; pick whichever fits the operator's business model. **None
   is "wrong."**
   - **Hard gate** — the app downloads freely from the Start9 registry,
     but won't function without a paid license. The binary is essentially
     a locked installer until the buyer activates. Common for closed-source
     paid apps and for open-source apps that the operator chooses to
     monetize through the registry distribution. See section 8 for the
     two flavors of hard gating (refuse-to-start vs. activate-screen-only).
   - **Soft gate** — the app runs and provides basic functionality
     unlicensed; specific paid features return 402 with an "Upgrade to
     unlock" message. Recommended for free → paid migrations and for
     freemium products.
   - **Nag mode** — no enforcement; just a "support development" banner
     when unlicensed. Pure honor system. Useful when the app is
     fundamentally free-to-use but the operator wants a tip-jar.

   Nudge the operator if their answer doesn't match their business
   reality. Closed-source-paid + nag-mode is incoherent; freemium +
   hard-gate alienates the existing user base.
5. **What are the entitlement strings, and what does each unlock?**
   The operator decides; ask them. Common patterns:
   - `["self_host"]` for a free tier — "you can run the app, no premium features"
   - `["self_host", "export", "ai_features", "team_seats"]` for a paid tier
   - `["patron"]` extra for a vanity supporter tier
   Document the mapping (entitlement string → feature unlocked) in your
   integration so the operator can ship the right policies.
6. **Where should the license key live on disk at runtime?**
   Default: `/data/license.txt` for server / containerized apps, or
   `~/.config/<your-app>/license.key` for desktop apps. Operator may
   override.
7. **Which pricing tiers exist** and roughly what they cost? (Optional
   for the integration itself, but useful for shaping the "Upgrade"
   message that shows when an unlicensed user hits a paid feature.)

   Two-or-more-tier products unlock a UX option: an **in-app tier
   picker** that renders the buyer's options inside the operator's
   own UI (e.g. on the activation screen) and drives the purchase
   programmatically through the SDK, instead of redirecting to the
   externally-hosted `/buy/<slug>` page. See section 11a — this is
   often the strongest fit when the app already has a settings or
   activation surface where "Choose a plan" feels native. If there's
   only one tier (or only one *paid* tier), skip this and use the
   simpler single-policy flow in section 11.

If the creator doesn't know yet, propose sensible defaults from the
ranges above and confirm before coding.

8. **Compile a config card before writing code.** After answering 1–7,
   produce a short summary the operator can paste into the Keysat admin
   without re-deriving anything. This is the single highest-leverage
   step for avoiding "wait, what entitlements did we agree on?" churn
   later. The card has three parts:

   - **Product**: the slug from question 2.
   - **Policies**: each policy's name and the entitlement set it issues.
     Treat this as the operator's pricing menu — one policy per tier.
   - **Behavior matrix**: caller state → what happens. Lets the operator
     sanity-check the gating model (question 4) against the policy set.

   Show the card to the operator, get explicit confirmation, *then*
   write code. Example for a two-tier hard-gate-flavor-2 freemium app:

   ```
   Product slug: youtube-summarizer

   Policies to create in Keysat admin:
     • Core   → entitlements: ["core"]
     • Pro    → entitlements: ["core", "subscriptions", "history", "library"]

   Entitlement → unlocks:
     core           — past the activation screen; basic summarize
     subscriptions  — channel subscriptions, auto-queue
     history        — saved summary library
     library        — bulk import/export

   Behavior matrix:
     no license     → 402 license_required everywhere
     Core license   → summarize works; subs/history/library = 402 feature_not_in_tier
     Pro license    → all features available
   ```

   Without this card, mid-implementation drift is near-certain — the LLM
   gates on `library_io`, the operator creates a policy with `library`,
   and the buyer sees a "feature not in tier" error on a feature they
   thought they paid for.

---

## 0a. How enforcement actually works (online vs offline)

This is the most-asked question every operator hits when they
realize they want to revoke a license, downgrade a buyer, or have
a recurring sub lapse. Read this section before designing your
gating logic; the choice you make here is sticky.

### What the buyer's app can enforce **offline**

These are baked into the **signed license key** at issuance time.
Once issued, they're cryptographically immutable for the life of
that key. The buyer can install your app on an air-gapped box and
these checks still work, forever:

- **Hard expiry.** If the operator issued the license with
  `duration_seconds: 31536000` (1 year), the offline verifier
  rejects it on day 366. No network needed.
- **Entitlement set.** Whatever entitlements were on the policy
  when the license was signed are what the offline check sees
  forever. Operator edits to the policy after issuance don't
  reach this license.
- **Trial flag.** TRIAL bit in the signed payload, offline
  detectable.
- **Fingerprint binding.** If the key was issued bound to
  machine X's fingerprint, machine Y fails offline verification.

These are tamper-proof because Ed25519 signatures can't be forged
without the operator's private key.

### What the operator can change **only via online enforcement**

These mutations live in the operator's licensing-service DB and
**never reach the buyer's app** unless the app actively calls
`/v1/validate`:

- **Revocation.** DB row flips `revoked_at`; signed key still
  verifies offline.
- **Tier downgrade / upgrade.** New entitlements live in the DB;
  signed key still has the old ones.
- **Recurring subscription lapse.** Sub goes `past_due` →
  `lapsed` server-side. Signed key (which is just `expires_at =
  now + 30 days` for monthly subs) keeps verifying offline until
  its baked expiry.
- **Seat enforcement** beyond per-key fingerprint binding.

### The two design dials the operator picks

For each product they sell:

1. **How short is the baked expiry?** Short (e.g. 35 days for a
   monthly sub) = buyer must come online frequently to refresh;
   operator retains tight control. Long / perpetual = buyer can
   stay offline indefinitely; operator gives up most post-sale
   enforcement.
2. **Does the buyer's app actually call `validate()`?** This is
   YOUR call as the SDK consumer. If the app only does
   `verifier.verify(key)` (offline signature check) and never
   calls `client.validate(...)`, **no operator-side change can
   ever reach a buyer who's already activated.** If the app calls
   `validate()` on launch + daily with a sensible cache fallback,
   operators have near-real-time control.

### The two patterns

**Pattern A — true perpetual, no take-backs.** App does
`verifier.verify(key)` at launch and trusts whatever the signed
payload says. Buyer pays once, gets entitlements forever, even
if the operator regrets it. Honest sale, like buying a Photoshop
CS6 disk in 2012. Works for: tools the operator is confident they
want to lifetime-license; markets where buyers explicitly value
"buy once, own forever"; software that may need to function
on air-gapped boxes.

**Pattern B — perpetual *price*, online-enforced entitlements.**
App calls `client.validate(...)` periodically (on launch + daily)
and treats the SERVER's entitlement set as authoritative. The
license is "perpetual" in that there's no expiry-driven re-payment,
but enforcement is live. Operator retains downgrade / revoke /
sub-lapse control. Buyer's offline experience is normal as long
as they come online once per cache window. This is what most
"SaaS replacement" products want.

```ts
// Pattern A — offline-only
import { Verifier } from '@keysat/licensing-client'
const v = new Verifier(OPERATOR_PUBKEY_PEM)
const ok = v.verify(licenseKey)
if (!ok.valid || !ok.entitlements.includes('core')) refuseToStart()

// Pattern B — online-aware with offline fallback
import { Client } from '@keysat/licensing-client'
const client = new Client(OPERATOR_KEYSAT_URL)
const result = await client.validate(licenseKey, { productSlug, fingerprint })
if (!result.ok) refuseToStart()
// result.entitlements is the LIVE set from the server
// On network failure, fall back to verifier.verify() with a
// cache TTL appropriate to your business (e.g. 7 days).
```

### Operator-side implication

Your pricing/enforcement model has to match the offline-vs-online
tradeoff:

- **Perpetual licenses** with Pattern A: you give up post-sale
  control. Honest sale. Refund-if-buyer-asks model.
- **Perpetual licenses** with Pattern B: full operator control,
  but the app has to be online periodically to bite. Buyers who
  go fully offline forever can't be touched.
- **Recurring subs**: NEED short baked-in expiries (1-2 cycles'
  worth) plus working `/v1/validate` integration. Otherwise
  lapsing is unenforceable.
- **Free trial converting to paid**: bake `expires_at = trial_end`
  so the trial expires offline, then renewal flow extends it on
  payment.

### What this means for the tier-upgrade feature (section 11a)

The whole tier-upgrade flow only has teeth if buyers' apps are
calling `validate()`. For a buyer using Pattern A who paid for
Patron and the operator later downgrades them: nothing happens
until they come online. **Same constraint going the other way:**
a Pattern A buyer's app wouldn't see new entitlements after an
upgrade until next online call.

This isn't a Keysat-specific limitation — it's a property of any
license model that doesn't require always-on phone-home. **Keysat
deliberately doesn't.** That's a feature, not a bug; but you, the
SDK consumer, need to decide which pattern your app implements
based on the operator's business model.

### Keysat dogfoods Pattern B

The Keysat daemon itself uses Pattern B for its own self-license:
verifies the on-disk LIC1 key at boot (Pattern A signature check),
THEN refreshes entitlements from the local DB hourly + on-demand
via `POST /v1/admin/self-license/refresh` (Pattern B online
component). This is the same pattern you'd implement in any
"perpetual price, live entitlements" app. See
`license_self::refresh_self_tier_from_db` for reference.

---

## 1. What Keysat does, in one paragraph

Keysat lets independent software creators sell their work on their own
terms. The operator (the creator) runs a Keysat instance — typically on
a Start9 box — and Keysat handles the buy page, the Bitcoin payment via
BTCPay, and issuing each buyer a signed license key in `LIC1-…-…` form.
**Your software's job** is to read that key from somewhere on disk
(a file, an env var, a config setting) and verify its signature against
the operator's public key. What happens after verification is up to the
creator: maybe the app refuses to function without a license (one-time
purchase model), maybe specific features unlock (free + paid tiers),
maybe nothing changes and the verified license is just used to show a
"thanks for supporting development" badge. You never talk to a Keysat
server at runtime unless you want to — verification is offline, fast
(~1ms), and doesn't depend on the network.

---

## 2. The whole integration in 30 seconds

```
1. Install the Keysat SDK in your language.
2. Embed the operator's PUBLIC key into your app at build time.
3. On startup, read the license key from disk; verify it; populate an
   `entitlements` set.
4. Throughout your code, gate paid features with `if entitlements.has("X")`.
5. (Optional) On a timer, also call /v1/validate to catch revocations.
```

Everything else is polish.

---

## 3. Prerequisites — three things you need from the operator

1. **A Keysat instance reachable on the public internet.** Typically
   something like `https://licensing.example.com`. The operator already
   has this; you don't need to install one.
2. **A product slug** the operator created in their Keysat. This is a
   short string (`acme-paint-pro`, `myapp`, etc.). Licenses issued for
   one slug won't validate against another — this is intentional and
   stops a customer from buying a cheap product and using its key to
   unlock an expensive one.
3. **The operator's signing public key in PEM form.** This is what you
   embed in source. Get it from:
   - The admin Overview tab → "Embed your public key" tip card → Copy
   - Or `curl https://licensing.example.com/v1/issuer/public-key | jq -r .public_key_pem`

   The PEM is non-secret — **anyone with the public key can verify
   licenses but not mint them.** It's safe to commit to source control
   and ship in your binary.

4. **The public buy URL for your product.** Each product on a Keysat
   instance has a buyer-facing page at
   `<keysat-base-url>/buy/<product-slug>`. Use this for "Buy a key" /
   "Upgrade to Pro" links in your app's activation screen, settings, and
   per-feature upsell tiles. Compute it from the same constants you've
   already embedded — don't hard-code a separate URL that can drift:

   ```ts
   const buyUrl = `${KEYSAT_BASE_URL.replace(/\/$/, "")}/buy/${PRODUCT_SLUG}`
   ```

   The simpler "link to a buy page" path (this URL) is fine for most
   apps. If you want a more integrated checkout, see section 11 for
   `client.startPurchase()`.

---

## 4. The wire format you'll be reading

License keys look like:

```
LIC1-AIAMCWOS5JVHSQE2UMP6PNKXODHSIPHM5O3XQQ2J6CE4XV6WVNMA3BIAAAAA…
```

A `LIC1-` prefix, then two base32 segments separated by `-`. The first
segment is a binary payload; the second is an Ed25519 signature over
the payload. The SDK parses and verifies in one call. You should never
need to handle the encoding manually.

The signed payload contains:
- `product_id` (UUID) — for matching against your product slug
- `license_id` (UUID) — useful for logging
- `issued_at` (Unix seconds)
- `expires_at` (Unix seconds; 0 means perpetual)
- `flags` (bitfield; `FLAG_TRIAL=1`)
- `entitlements: string[]` — **this is the array you gate features on**
- `fingerprint_hash` (32 bytes; for online machine-binding)

Your software reads `entitlements` and decides what to unlock.

---

## 5. Where to read the license from

There's no one-size-fits-all answer; pick one based on how your users
interact with your app. **Recommended order**:

1. **A file in the user's data directory.** On Linux this is typically
   `~/.config/<your-app>/license.key`, or `/data/license.txt` for
   server software running in a container. The file contains exactly
   one line: the raw `LIC1-…` string. This is the most common pattern.

2. **An environment variable** like `MYAPP_LICENSE_KEY`. Useful for
   server-side software, CLIs, Docker Compose, and systemd. Easy to
   set, but users forget they set it and lose track.

3. **A "paste your license key" UI** in your app's settings, with the
   value persisted to localStorage / OS keychain / your own config.
   Most familiar to users coming from commercial software.

4. **Multiple of the above.** A common pattern is: "env var first,
   then file, then UI prompt." All three give you a license string
   either way; the SDK doesn't care where it came from.

For Start9 packages: there's a [activate-license-template](./activate-license-template/)
that wires this up for you using StartOS Actions and the package store.
Copy that template, replace the slug, and you've got Pattern 1 + a
StartOS Actions UI for buyers to paste keys into.

---

## 6. The canonical integration pattern

Every integration follows the same shape regardless of language and
regardless of which enforcement model from question 4 the operator picked.
The verify-once-at-startup primitive is the same; what you do with the
result is what changes.

```
on startup:
    raw_key = read_license_string()             # file, env, or UI value
    license_state = {state: 'unlicensed', entitlements: []}
    if raw_key is not None:
        result = verify(raw_key, ISSUER_PEM)     # SDK call
        if result.is_valid:
            license_state = {
                state: 'licensed',
                entitlements: result.entitlements,
                license_id: result.license_id,
                expires_at: result.expires_at,
            }
        else:
            log("license rejected: " + result.reason)

    # Then — depending on the operator's chosen model:
    #
    #   HARD GATE  : if not licensed, exit (Flavor 1) or block all
    #                business endpoints (Flavor 2). See section 7d.
    #
    #   SOFT GATE  : run normally; specific feature handlers consult
    #                license_state.entitlements before unlocking.
    #                See section 7a/7b/7c.
    #
    #   NAG MODE   : run normally; show a "support development" banner
    #                in the UI when license_state.state != 'licensed'.
```

The verify-and-populate-state step is identical for all three models.
The doc is structured the same way: section 7 covers the verify
primitive in each language; section 7d covers the hard-gate enforcement
flavors; the worked examples in section 14 show soft-gate; the
patterns are mix-and-match.

**One universal rule across all three models:** never hard-fail on
*network* errors during the optional online `validate()` call (section 9).
That's separate from refusing to start when no license is present — which
is fine for hard-gate Flavor 1. The thing to avoid is making your app's
uptime depend on the operator's licensing server being reachable.

**Don't forget background workers.** HTTP middleware gates only catch
incoming requests. If you have in-process timers, schedulers, queue
consumers, or other background jobs that exercise gated features, add
an explicit early-return at the top of each one:

```js
async function checkSubscriptionsBackground() {
  if (!LIC.entitlements.has("subscriptions")) return  // skip silently
  // … existing work
}
```

Otherwise an unlicensed (or insufficient-tier) instance will keep doing
work the buyer didn't pay for — wasting bandwidth, API quota, and
server CPU, and producing stale state in the UI when entitlements are
later restored. This bites people because the server returns 402 to
direct callers but the timer keeps humming along.

---

## 7. Language-specific implementations

### 7a. TypeScript / Node

**Install (preferred, once published):**

```bash
npm install @keysat/licensing-client
```

**GitHub fallback** (if the npm package isn't published yet). Several
prerequisites must be met for this path to work end-to-end:

1. The `keysat-xyz/keysat-client-ts` repo must be **public** on GitHub.
   Private repos require credentials, which fails inside hermetic build
   environments (Docker, CI, fresh dev machines without an SSH key). If
   the repo flips public temporarily for one build, every future build
   re-hits this wall — prefer publishing to npm if at all possible.
2. The repo must include a `prepare` script in `package.json` that
   builds `dist/` on git-install. This is fixed as of this doc; if you
   see `Cannot find module '...dist/index.cjs'` after install, the SDK
   you're pulling pre-dates the fix and you need a newer commit.
3. **Use the explicit `git+https://` URL form**, not the `github:`
   shorthand:

   ```jsonc
   // package.json
   "@keysat/licensing-client": "git+https://github.com/keysat-xyz/keysat-client-ts.git"
   ```

   The `github:user/repo` shorthand often resolves to `git+ssh://...`
   on machines with an existing GitHub SSH key, which then breaks for
   any subsequent integrator without a key (CI, Docker, a fresh laptop).

4. **If you switched from `github:` to `git+https://`, also delete the
   stale lock-file entry.** `npm install` will keep the previous
   `resolved: "git+ssh://..."` line in `package-lock.json` even after
   you change the spec in `package.json`. The fastest fix is:

   ```bash
   rm package-lock.json node_modules
   npm cache clean --force
   npm install
   ```

   Or hand-edit the `resolved:` field of the offending entry to swap
   `git+ssh://` → `git+https://`, leaving the commit hash unchanged.

When all four are satisfied:

```bash
npm install github:keysat-xyz/keysat-client-ts
```

**Embed the public key.** The simplest way is to commit the PEM file
to your repo at `assets/issuer.pub` and import it as a raw string:

```ts
// in your bundler config (Vite shown)
import issuerPem from './assets/issuer.pub?raw'
```

Or in plain Node:

```ts
import { readFileSync } from 'node:fs'
import * as path from 'node:path'
const issuerPem = readFileSync(path.join(__dirname, 'assets/issuer.pub'), 'utf8')
```

**Verify on startup:**

```ts
import { Verifier, PublicKey } from '@keysat/licensing-client'
import { readFileSync } from 'node:fs'

const PRODUCT_SLUG = '<your-product-slug>'
const LICENSE_PATH = process.env.MYAPP_LICENSE_KEY_PATH || '/data/license.txt'

function readLicenseKey(): string | null {
  if (process.env.MYAPP_LICENSE_KEY) return process.env.MYAPP_LICENSE_KEY.trim()
  try { return readFileSync(LICENSE_PATH, 'utf8').trim() }
  catch { return null }
}

const verifier = new Verifier(PublicKey.fromPem(issuerPem))

export interface LicenseState {
  state: 'licensed' | 'unlicensed' | 'invalid'
  reason?: string
  licenseId?: string
  entitlements: Set<string>
  expiresAt?: Date
  isTrial?: boolean
}

export function checkLicense(): LicenseState {
  const raw = readLicenseKey()
  if (!raw) return { state: 'unlicensed', entitlements: new Set() }
  try {
    const ok = verifier.verify(raw)
    // (optional) reject keys for the wrong product slug
    if (ok.payload.productSlug && ok.payload.productSlug !== PRODUCT_SLUG) {
      return { state: 'invalid', reason: 'product_mismatch', entitlements: new Set() }
    }
    return {
      state: 'licensed',
      licenseId: ok.payload.licenseId,
      entitlements: new Set(ok.payload.entitlements || []),
      expiresAt: ok.payload.expiresAt
        ? new Date(ok.payload.expiresAt * 1000)
        : undefined,
      isTrial: !!(ok.payload.flags & 1),
    }
  } catch (e: any) {
    return { state: 'invalid', reason: e.message, entitlements: new Set() }
  }
}
```

**Use the state object** wherever a feature is gated:

```ts
const lic = checkLicense()
console.log(`[license] state=${lic.state} entitlements=[${[...lic.entitlements].join(',')}]`)

// In an Express route:
app.post('/api/export', (req, res) => {
  if (!lic.entitlements.has('export')) {
    return res.status(402).json({
      error: 'feature_not_in_tier',
      message: 'Export requires a paid license. See <upgrade_url>.',
    })
  }
  // ... existing export logic
})
```

### 7b. Python

**Install (preferred, once published):**

```bash
pip install keysat-licensing-client
```

**GitHub fallback** (if the PyPI package isn't published yet). The
`keysat-xyz/keysat-client-python` repo must be **public** on GitHub
for this to work in clean environments:

```bash
pip install git+https://github.com/keysat-xyz/keysat-client-python.git
```

(Python's pip-from-git path is simpler than npm's — no separate build
step is required since pure-Python packages are installable from source.)

**Embed the public key** at a path your code can read:

```python
# myapp/license.py
from pathlib import Path
ISSUER_PEM = (Path(__file__).parent / 'assets' / 'issuer.pub').read_text()
```

**Verify on startup:**

```python
# myapp/license.py
import os
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Optional

from keysat_licensing_client import Verifier

PRODUCT_SLUG = '<your-product-slug>'
LICENSE_PATH = os.environ.get('MYAPP_LICENSE_KEY_PATH', '/data/license.txt')

ISSUER_PEM = (Path(__file__).parent / 'assets' / 'issuer.pub').read_text()
_verifier = Verifier.from_pem(ISSUER_PEM)


@dataclass
class LicenseState:
    state: str  # 'licensed' | 'unlicensed' | 'invalid'
    reason: Optional[str] = None
    license_id: Optional[str] = None
    entitlements: set = field(default_factory=set)
    expires_at: Optional[datetime] = None
    is_trial: bool = False


def _read_license_key() -> Optional[str]:
    if env := os.environ.get('MYAPP_LICENSE_KEY'):
        return env.strip()
    try:
        return Path(LICENSE_PATH).read_text().strip()
    except (FileNotFoundError, PermissionError):
        return None


def check_license() -> LicenseState:
    raw = _read_license_key()
    if not raw:
        return LicenseState(state='unlicensed')
    try:
        ok = _verifier.verify(raw)
        return LicenseState(
            state='licensed',
            license_id=str(ok.payload.license_id),
            entitlements=set(ok.payload.entitlements or []),
            expires_at=datetime.fromtimestamp(ok.payload.expires_at)
                if ok.payload.expires_at else None,
            is_trial=bool(ok.payload.flags & 1),
        )
    except Exception as e:
        return LicenseState(state='invalid', reason=str(e))
```

**Use it:**

```python
# myapp/server.py
from .license import check_license

LIC = check_license()
print(f'[license] state={LIC.state} entitlements={LIC.entitlements}')

@app.post('/api/export')
def export_endpoint():
    if 'export' not in LIC.entitlements:
        abort(402, description={
            'error': 'feature_not_in_tier',
            'message': 'Export requires a paid license.',
        })
    # ... do the thing
```

### 7c. Rust

**Install (preferred, once published):**

```toml
# Cargo.toml
[dependencies]
keysat-licensing-client = "0.1"
```

**Git fallback** (if not on crates.io yet). The
`keysat-xyz/keysat-client-rust` repo must be **public** on GitHub:

```toml
keysat-licensing-client = { git = "https://github.com/keysat-xyz/keysat-client-rust.git" }
```

Cargo builds from source, so no separate build step is required.

**Embed the public key:**

```rust
const ISSUER_PEM: &str = include_str!("../assets/issuer.pub");
```

**Verify on startup:**

```rust
// src/license.rs
use keysat_licensing_client::{Verifier, PublicKeyPem};
use std::collections::HashSet;
use std::path::PathBuf;

pub const PRODUCT_SLUG: &str = "<your-product-slug>";
pub const ISSUER_PEM: &str = include_str!("../assets/issuer.pub");

#[derive(Debug, Clone)]
pub struct LicenseState {
    pub state: &'static str, // "licensed" | "unlicensed" | "invalid"
    pub reason: Option<String>,
    pub license_id: Option<String>,
    pub entitlements: HashSet<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_trial: bool,
}

impl Default for LicenseState {
    fn default() -> Self {
        Self {
            state: "unlicensed",
            reason: None,
            license_id: None,
            entitlements: HashSet::new(),
            expires_at: None,
            is_trial: false,
        }
    }
}

fn read_license_key() -> Option<String> {
    if let Ok(s) = std::env::var("MYAPP_LICENSE_KEY") {
        let s = s.trim().to_string();
        if !s.is_empty() { return Some(s) }
    }
    let path = std::env::var("MYAPP_LICENSE_KEY_PATH")
        .unwrap_or_else(|_| "/data/license.txt".to_string());
    std::fs::read_to_string(PathBuf::from(path))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn check_license() -> LicenseState {
    let raw = match read_license_key() {
        Some(s) => s,
        None => return LicenseState::default(),
    };
    let pubkey = match PublicKeyPem::from_str(ISSUER_PEM) {
        Ok(k) => k,
        Err(e) => return LicenseState {
            state: "invalid", reason: Some(format!("bad pubkey embedded: {e}")),
            ..Default::default()
        },
    };
    let verifier = Verifier::new(pubkey);
    match verifier.verify(&raw) {
        Ok(ok) => LicenseState {
            state: "licensed",
            license_id: Some(ok.payload.license_id.to_string()),
            entitlements: ok.payload.entitlements.into_iter().collect(),
            expires_at: if ok.payload.expires_at == 0 {
                None
            } else {
                chrono::DateTime::from_timestamp(ok.payload.expires_at, 0)
            },
            is_trial: (ok.payload.flags & 1) != 0,
            ..Default::default()
        },
        Err(e) => LicenseState {
            state: "invalid", reason: Some(e.to_string()),
            ..Default::default()
        },
    }
}
```

**Use it:**

```rust
let lic = license::check_license();
tracing::info!(state = lic.state, entitlements = ?lic.entitlements, "license loaded");

// At a feature gate:
if !lic.entitlements.contains("export") {
    return Err(MyError::PaymentRequired(
        "Export requires a paid license.".into()
    ));
}
```

### 7d. Hard-gate patterns — "the app doesn't function without a license"

If the operator chose **hard gate** in the section-0 questions (binary
freely downloadable, but locked until activated), use one of these two
flavors instead of the entitlements-as-feature-flags pattern above. The
verifier helpers from 7a / 7b / 7c are still the right primitive — the
difference is what you do with the result.

**Flavor 1: Refuse to start.** The daemon exits at boot with a clear
log line if there's no valid license. StartOS will show the service as
crashing — the operator's README needs to tell buyers "install the
license first via Actions → Set license, then start the service."

```ts
// TypeScript / Node
const lic = checkLicense()
if (lic.state !== 'licensed') {
  console.error(`[license] not licensed (${lic.state}): ${lic.reason || ''}`)
  console.error(`[license] paste a license key into ${LICENSE_PATH} via the StartOS "Set license" action, then restart.`)
  process.exit(1)
}
```

```python
# Python
lic = check_license()
if lic.state != 'licensed':
    log.error(f'[license] not licensed ({lic.state}): {lic.reason or ""}')
    log.error(f'[license] paste a license key, then restart.')
    raise SystemExit(1)
```

```rust
// Rust
let lic = license::check_license();
if lic.state != "licensed" {
    eprintln!("[license] not licensed ({}): {}", lic.state, lic.reason.unwrap_or_default());
    eprintln!("[license] paste a license key, then restart.");
    std::process::exit(1);
}
```

This is the most aggressive option. Use when (a) the app is closed-source
and there's no "free version" of the binary anyone could compile, and
(b) the operator is OK with StartOS surfacing the service as
unhealthy until activated.

**Flavor 2: Run, but block all real work behind an "Activate" screen.**
The daemon starts normally, but every business endpoint returns 402
until a license is activated. Only the activation endpoint(s) and a
status endpoint are open. Buyers see a clean "paste your license to
get started" UI on first run; StartOS shows the service as healthy.
Generally a better buyer experience than Flavor 1.

```ts
// TypeScript / Express — middleware that gates everything except
// the activation paths.
const ACTIVATION_PATHS = new Set([
  '/api/license-status',  // for the frontend to render activation UI
  '/api/activate',         // accepts a pasted license key, writes to file, refreshes state
  '/healthz',              // for StartOS / orchestration
])

let LIC = checkLicense()  // mutable; refresh after activation

app.use((req, res, next) => {
  if (ACTIVATION_PATHS.has(req.path)) return next()
  if (LIC.state !== 'licensed') {
    return res.status(402).json({
      error: 'license_required',
      message: 'This service requires a Keysat license to function.',
      activate_url: '/activate',  // your frontend's activation page
      state: LIC.state,
      reason: LIC.reason,
    })
  }
  next()
})

// Activation endpoint — accepts a pasted key, writes it, re-checks.
app.post('/api/activate', express.json(), (req, res) => {
  const key = (req.body.license_key || '').trim()
  if (!key.startsWith('LIC1-')) {
    return res.status(400).json({ error: 'bad_format', message: 'Expected a LIC1-… key.' })
  }
  fs.writeFileSync(LICENSE_PATH, key + '\n')
  LIC = checkLicense()
  if (LIC.state === 'licensed') {
    return res.json({ ok: true, state: 'licensed', entitlements: [...LIC.entitlements] })
  }
  return res.status(400).json({ error: 'invalid', state: LIC.state, reason: LIC.reason })
})
```

```python
# Python / Flask — same idea
ACTIVATION_PATHS = {'/api/license-status', '/api/activate', '/healthz'}
LIC = check_license()  # module-level; reload after activation

@app.before_request
def license_gate():
    if request.path in ACTIVATION_PATHS:
        return None
    if LIC.state != 'licensed':
        return jsonify({
            'error': 'license_required',
            'message': 'This service requires a Keysat license to function.',
            'state': LIC.state,
            'reason': LIC.reason,
        }), 402

@app.post('/api/activate')
def activate():
    global LIC
    key = (request.json or {}).get('license_key', '').strip()
    if not key.startswith('LIC1-'):
        return {'error': 'bad_format'}, 400
    Path(LICENSE_PATH).write_text(key + '\n')
    LIC = check_license()
    if LIC.state == 'licensed':
        return {'ok': True, 'entitlements': sorted(LIC.entitlements)}
    return {'error': 'invalid', 'state': LIC.state, 'reason': LIC.reason}, 400
```

```rust
// Rust / axum — same idea: a middleware layer that guards all admin
// routes, plus an /api/activate endpoint that accepts a key and updates
// the in-memory state.
//
// Sketch (full impl follows the existing axum middleware pattern):
//   Router::new()
//     .route("/api/license-status", get(license_status))
//     .route("/api/activate", post(activate))
//     .route("/healthz", get(healthz))
//     .nest("/api", api_routes())
//     .layer(axum::middleware::from_fn_with_state(state.clone(), license_gate))
//
// Inside `license_gate`, return 402 unless the request path is in
// ACTIVATION_PATHS or `state.license.read().await.state == "licensed"`.
```

**How would Keysat itself do this?** Keysat already has the `Mode::Enforce`
build-time flag in [`license_self.rs`](./licensing-service-startos/licensing-service/src/license_self.rs):
when built with `KEYSAT_LICENSE_ENFORCE=1`, missing or invalid licenses
cause the daemon to refuse to start (Flavor 1). Default Permissive
builds run unlicensed at Creator-tier caps. To switch Keysat to Flavor 2
("run but block until activated") would mean: keep the existing boot-time
license check non-fatal, expose `/admin/login`-style activation endpoints
under a hardcoded allowlist, and have an axum middleware return 402 on
every other admin/business endpoint until `state.self_tier` flips from
`Unlicensed` to `Licensed`. The pieces are all there — it's a few hundred
lines of axum middleware + an SPA "Activate" splash screen.

### 7e. Packaging gotchas — Docker, s9pk, hermetic builds

Most non-trivial integrations end up packaged in Docker (Start9 s9pk,
generic container deploys, CI-built images). The following gotchas
together account for ~80% of the "it works locally but the build
fails" failure mode:

**1. Slim base images don't ship `git`, `ssh`, or `ca-certificates`.**
`node:20-slim`, `python:3.11-slim`, etc. are intentionally minimal.
If you have a git-URL dependency (e.g. the GitHub fallback above),
you'll need at least these in the *builder* stage:

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
      git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
```

Without `git`: npm errors with `spawn git ENOENT` when resolving the
dependency. Without `ca-certificates`: HTTPS clones fail with
`SSL certificate problem: unable to get local issuer certificate`.

**2. npm's git resolver tries `ssh://` for github.com URLs first.**
Even if your `package.json` spec and `package-lock.json` `resolved`
both say `git+https://`, npm internally tries SSH first when the host
is github.com. In a container with no SSH client or key, this fails.
Force git to silently rewrite SSH URLs to HTTPS:

```dockerfile
RUN git config --global --add url."https://github.com/".insteadOf "ssh://git@github.com/" \
 && git config --global --add url."https://github.com/".insteadOf "git@github.com:" \
 && git config --global --add url."https://github.com/".insteadOf "git://github.com/"
```

The `--add` flag matters — without it, each subsequent invocation
overwrites the previous one (they share a key) and only the last
rewrite is active.

**3. Don't forget to `COPY` your new license module.** If your
Dockerfile lists individual server files explicitly:

```dockerfile
COPY server/package.json ./server/
COPY server/index.js ./server/
COPY public/ ./public/
COPY assets/ ./assets/
```

…the build will succeed, the image will start, and then crash at
runtime with `Cannot find module './license.js'`. Add a line for the
license module:

```dockerfile
COPY server/license.js ./server/   # ← easy to miss
```

This is the single most common "package builds, container won't boot"
failure when retro-fitting licensing into an existing app.

**4. Make's incremental rebuild can mask uncommitted changes.** s9pk
build chains often look like `make x86 → start-cli s9pk pack → docker
build`. Make may decide nothing's newer than the existing `.s9pk`
because its dependencies typically include `.git/index` (which only
updates on `git add`). Symptom: you change a source file, rebuild,
get an instant "✅ Build Complete!" with the same package as before.

Either stage your changes (`git add -A`) so `.git/index` updates, or
delete the existing `.s9pk` to force a rebuild:

```bash
rm myapp_x86_64.s9pk && make x86
```

**5. The `--ignore-scripts` flag will skip the SDK's `prepare` build.**
If your Dockerfile uses `npm ci --ignore-scripts` (a common security
hardening), the SDK won't build its `dist/` and you'll hit the
"Cannot find module" runtime error from §7a. Either drop
`--ignore-scripts` for the builder stage, or pre-build the SDK
elsewhere and vendor `dist/` in.

### 7f. Frontend integration for hard-gate Flavor 2

If you picked hard-gate Flavor 2 (server starts, business endpoints
return 402 until activated), **the frontend is half the work** —
otherwise unlicensed users see a sea of fetch errors instead of a
clean activation screen. The pattern below is framework-agnostic and
works in vanilla JS, React, Vue, etc.

**Step 1: Fetch license-status before any other API call.** It's the
prerequisite for deciding what to render.

```js
async function loadLicenseStatus() {
  const r = await fetch("/api/license-status")
  return r.json()  // { state, entitlements, productSlug, keysatBaseUrl, … }
}
```

**Step 2: Render the activation screen as a top-level guard.** If
`state !== "licensed"` (or the `core` entitlement is missing), replace
the entire app body with the activation card. Don't render the normal
UI underneath — every API call would 402 anyway, producing visible
broken state.

```jsx
if (lic.state !== "licensed" || !lic.entitlements.includes("core")) {
  return <ActivationScreen lic={lic} onActivate={key => activate(key)} />
}
return <App />
```

**Step 3: The activation card needs four things:**
- A `<textarea>` for pasting the LIC1-... key (use a textarea, not an
  input — keys are 100+ chars and users will copy-paste with
  whitespace)
- An "Activate" button that POSTs to `/api/license/activate` with
  `{license_key: <pasted>}` and refreshes state on success
- Distinct error messages for each `reason` code (see §12), not a
  generic "activation failed"
- A "Buy a key" link to `${keysatBaseUrl}/buy/${productSlug}` (see §3)

**Optional — embed the tier picker directly in the activation card.**
For multi-tier products, instead of (or in addition to) the "Buy a
key" link, render an inline tier picker that lets the buyer pay
without leaving your app. Calls
`Client.listPublicPolicies(productSlug)` to render the tier list and
`Client.startPurchase(productSlug, { policySlug })` to drive the
checkout. The full pattern, including the architecture diagram and
common mistakes, is in **section 11a**. This is the pattern Recap
ships in their activation screen.

**Step 4: Gate Pro features in the UI, not just the server.** The
server returns 402 for missing entitlements, but unless the frontend
also checks, users see ghost UI for features they can't use:

```jsx
{lic.entitlements.includes("subscriptions")
  ? <SubscriptionsPanel />
  : <ProUpsell feature="subscriptions" buyUrl={buyUrl} />}
```

Each `ProUpsell` should explain what they'd unlock, not just "Pro
feature." The server's 402 response includes a `message` field with a
sentence-long description — surface it.

**Step 5: Add a license block to settings.** Buyers want to see what
tier they're on, when it expires, and have a way to remove the key.
Hit `/api/license-status` for state, render a colored badge per tier,
and expose a "Deactivate" button that POSTs to
`/api/license/deactivate`.

**Step 6: Respond to entitlement changes without a reload.** After
activation, re-fetch any data your app skipped at boot (history,
subscriptions, etc.) — the user just unlocked them. After
deactivation, clear it from in-memory state so the previous tier's
data doesn't leak through the activation screen.

**Reference shape your `/api/license-status` should return** so the
frontend has everything it needs without extra round-trips:

```json
{
  "state": "licensed",
  "reason": null,
  "licenseId": "abc123…",
  "entitlements": ["core", "subscriptions", "history", "library"],
  "expiresAt": "2027-05-01T00:00:00Z",
  "isTrial": false,
  "productSlug": "youtube-summarizer",
  "keysatBaseUrl": "https://licensing.example.com"
}
```

`productSlug` and `keysatBaseUrl` aren't strictly part of license
state — they're there so the frontend can construct the `/buy/<slug>`
URL without hard-coding it. Ship them in the response.

---

## 8. Picking entitlement names

Entitlement strings are arbitrary; they're whatever the operator put on
the policy when issuing the license. Common conventions:

- **Feature flags**: `export`, `ai_summaries`, `team_seats`, `recurring_billing`, `card_payments`
- **Capability tiers**: `unlimited_products`, `unlimited_seats`, `priority_support`
- **Branded markers**: `patron` (no real feature, just a badge)

Pick names that are stable, lowercase, snake-case, descriptive. Document
your chosen entitlement names in your README so operators / customers
know what they're buying. Treat them like API contract — once you ship
a feature gated on `"export"`, you can't rename to `"file_export"` without
breaking existing licenses.

The operator can use whatever set they want when creating policies; your
app only needs to know the names of features it gates on. Operators
selling tiered plans typically have:
- A free / Creator tier with one entitlement (`self_host` or similar)
- A pro / paid tier with several (`unlimited_*`, premium features)
- Optional Patron / supporter tier with all of Pro plus a `patron` badge

### Entitlements catalog (v0.2.0:8+)

Operators can declare a closed list of entitlements per product
in admin (Products → Edit → "Entitlements catalog"). Each entry has
three fields:

```
slug          name              description
core          Core              Past the activation screen, basic features.
ai_summaries  AI summaries      Auto-generate per-video summaries with GPT.
library_io    Library I/O       Bulk import/export of saved summaries.
```

Once a catalog exists for a product, two things change:

1. **The policy editor switches** from a free-text textarea to a
   click-to-toggle bubble picker that only offers entitlements from
   the catalog. The daemon enforces this at write time too (closed
   list).
2. **The buy page renders display names + descriptions** instead of
   raw slugs. Buyers see "AI summaries" with the description as a
   hover tooltip, never the underscore-laden `ai_summaries`.

For your SDK integration, the catalog comes back on
`GET /v1/products/<slug>/policies` (and equivalently
`Client.listPublicPolicies()` in all four SDKs):

```ts
const { product, policies } = await client.listPublicPolicies(SLUG)
// product.entitlementsCatalog is EntitlementDef[]:
//   [{ slug: 'ai_summaries', name: 'AI summaries', description: '...' }, ...]
//
// Use it to render an in-app tier picker that shows the same human-
// readable names the buy page does:
function entitlementLabel(slug: string): string {
  const def = product.entitlementsCatalog.find((e) => e.slug === slug)
  return def?.name || slug.replace(/_/g, ' ')
}
```

If the operator hasn't defined a catalog (legacy "free-text" mode),
the array is empty and you fall back to rendering the raw slugs —
or replacing underscores with spaces yourself for a quick polish.

**Catalog stability rule**: once you ship gating logic that checks
for entitlement `"export"`, the operator's catalog and policy
references have to stay using `"export"`. Renaming the slug breaks
existing licenses (which carry the old slug in their signed
payload). Adding NEW entitlement slugs to the catalog is fine —
just not renaming or deleting ones that licenses already reference.

---

## 9. Online validation (optional, recommended)

Offline verify proves the key was signed by the right operator. **Online
validation also catches revocations** (operator disabled a key) and
**enforces fingerprint binding** (one license = one machine). Use both:
offline at boot, online on a timer.

```ts
// TypeScript
import { Client } from '@keysat/licensing-client'
const client = new Client('https://licensing.example.com')

async function onlineCheck(licenseKey: string, machineFingerprint: string) {
  try {
    const r = await client.validate(licenseKey, PRODUCT_SLUG, machineFingerprint)
    if (!r.ok) {
      // r.reason is one of: 'revoked' | 'fingerprint_mismatch' |
      //                     'not_found' | 'bad_signature' | 'product_mismatch'
      console.warn('license rejected:', r.reason)
      // → React in your UI; don't hard-crash on this.
    }
  } catch {
    // Network errors → "status unknown". Don't block the user.
  }
}
```

```python
# Python
from keysat_licensing_client import Client
client = Client('https://licensing.example.com')
try:
    r = client.validate(license_key, PRODUCT_SLUG, machine_fingerprint)
    if not r.ok:
        log.warning(f'license rejected: {r.reason}')
except Exception:
    pass  # network error → status unknown
```

```rust
// Rust (with `online` feature)
let client = keysat_licensing_client::online::Client::new("https://licensing.example.com")?;
match client.validate(&key, Some(PRODUCT_SLUG), Some(&fp)).await {
    Ok(r) if r.ok => { /* fine */ }
    Ok(r) => log::warn!("rejected: {:?}", r.reason),
    Err(_) => { /* network error, don't punish */ }
}
```

**Cadence**: once at startup after the offline check succeeds, then on
a timer (hourly is plenty). Once-per-feature-call is too aggressive
and beats up the operator's server.

**Critical**: never refuse to start if `validate()` throws. Network
errors must degrade to "I can't tell, assume the user is fine" — not
"app refuses to launch." Otherwise your app's uptime depends on the
operator's licensing server being up.

---

## 10. Fingerprint binding (for `validate()`)

When you call `client.validate(...)`, the third argument is a machine
fingerprint. The operator's Keysat binds the first fingerprint it sees
to the license; subsequent calls with a different fingerprint return
`reason: 'fingerprint_mismatch'`. This is the anti-piracy mechanism.

**Compute the fingerprint** from something stable across reboots but
unique per machine:

| Platform | Source |
|---|---|
| Linux | `/etc/machine-id` |
| macOS | `ioreg -d2 -c IOPlatformExpertDevice` → IOPlatformUUID |
| Windows | Registry: `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` |
| Fallback | A UUID written into your app's config dir on first launch |

Mix in a per-product salt so fingerprints collected by your app can't be
reused against a different operator's licensing service:

```ts
const fingerprint = `${PRODUCT_SLUG}|${machineId}`
```

The SDK hashes this before sending, so the operator's Keysat never sees
the raw input.

If a customer legitimately moves devices and hits `fingerprint_mismatch`,
they should contact the operator. The operator can reset the binding
from their admin dashboard. Don't try to help users bypass this in
your app — it's the protection working as intended.

---

## 11. Driving the purchase flow from inside your app (optional)

If your app can open URLs (desktop GUI, CLI that can `xdg-open`), you
can drive the entire purchase flow from inside without forcing the user
into a separate browser tab.

```ts
import { Client } from '@keysat/licensing-client'
import open from 'open'

const client = new Client('https://licensing.example.com')

async function buyLicense(buyerEmail?: string): Promise<string> {
  const session = await client.startPurchase(PRODUCT_SLUG, { buyerEmail })
  await open(session.checkoutUrl) // BTCPay invoice page
  const key = await client.waitForLicense(session.invoiceId, { timeoutMs: 30 * 60_000 })
  return key
}
```

`waitForLicense` polls `/v1/purchase/<id>` until the BTCPay invoice
settles and the license is signed. Save the returned key to disk
(`/data/license.txt` or wherever your app reads from), then re-run
`checkLicense()`.

The simpler alternative: just link to the operator's buy page and let
them complete the purchase on the web, then paste the resulting key
into your app's settings. Less integrated, less friction to implement.

If the product has **two or more public policies** (Core/Pro, Free/
Standard/Pro, etc.), see section 11a for the tier-aware flow that
lets buyers pick a tier inside your app's own UI.

---

## 11a. Tier-aware purchases — in-app tier picker (multi-tier products)

When a product has multiple public policies, the buyer needs to **pick
which tier they're paying for** before the invoice is created. Section
11's `startPurchase(slug, { buyerEmail })` defaults to the product's
"default" policy (or the first active one), which works fine for
single-tier products but always issues a Core license on a Core/Pro
setup — no matter what the buyer wanted.

The fix has two pieces, both supported by the SDK since 0.2.0:

1. **`Client.listPublicPolicies(productSlug)`** — fetches the buyer-
   visible tier list from `GET /v1/products/<slug>/policies`. Public
   endpoint, no auth. Returns each tier's slug, display name, price
   (in the product's listed currency's smallest unit — sats for SAT,
   cents for USD/EUR), entitlements, recurring/trial flags, and the
   "Most popular" highlight flag. Render this into your tier-picker
   UI; it'll stay in sync if the operator adds/edits tiers in Keysat
   admin without you redeploying the app.
2. **`policySlug` field on `startPurchase`'s options** — when set, the
   licensing service prices the invoice at that policy's
   `price_sats_override` and the issued license carries that policy's
   entitlements, duration, max_machines, and trial flag.

### When you'd use this

- Multi-tier products where the choice happens in the buyer's app
  (activation screen, settings, in-app upgrade banner). Common shape:
  freemium app where Free is gated by `core` entitlement and Pro
  unlocks `subscriptions, history, library`.
- Operators who want to add or rename tiers without forcing an app
  update — the picker rebuilds itself off `listPublicPolicies`.
- Apps that need to write the issued license key directly to disk
  themselves (e.g. via a backend service, not via copy-paste from
  the buy page). The SDK delivers the signed key as a string; you
  write it where you want.

### Pattern (TypeScript / web app frontend)

```ts
import { Client, PublicPolicy } from '@keysat/licensing-client'

const client = new Client('https://licensing.example.com')

// 1. Fetch tiers — typically on activation screen mount.
const { product, policies } = await client.listPublicPolicies(PRODUCT_SLUG)

// 2. Render `policies` into your tier-picker UI. Each policy carries
//    everything you need to display:
function renderTier(p: PublicPolicy) {
  return `
    <button data-slug="${p.slug}" class="tier ${p.highlighted ? 'popular' : ''}">
      <h3>${p.name}</h3>
      <p>${p.description}</p>
      <div class="price">${formatPrice(p.priceSats, product /* for currency */)}
                         ${p.isRecurring ? '/' + cadence(p.renewalPeriodDays) : ''}</div>
      <ul>${p.entitlements.map(e => `<li>${e}</li>`).join('')}</ul>
    </button>`
}

// 3. Buyer picks a tier; you call startPurchase with policySlug.
async function buyTier(chosenSlug: string, buyerEmail: string) {
  const session = await client.startPurchase(PRODUCT_SLUG, {
    policySlug: chosenSlug,            // <-- the discriminator
    buyerEmail,
    redirectUrl: 'https://your-app.example/thank-you',
  })

  // 4. Open the checkout URL. For desktop apps, `open(session.checkoutUrl)`.
  //    For web apps, `window.location.href = session.checkoutUrl`.
  window.location.href = session.checkoutUrl

  // 5. After payment settles, your backend (or the buyer's poll) hits
  //    /v1/purchase/<invoice_id> and gets the signed license_key.
  //    Write it to wherever your app reads from. Reload validate.
}
```

### Pattern (other languages — same shape)

```python
# Python
from keysat_licensing_client import Client, StartPurchaseOptions

client = Client('https://licensing.example.com')
tiers = client.list_public_policies(PRODUCT_SLUG)

# render tiers.policies in your UI; user picks "pro"
session = client.start_purchase(PRODUCT_SLUG, StartPurchaseOptions(
    policy_slug='pro',
    buyer_email='buyer@example.com',
))
# open session.checkout_url; poll on settle
key = client.wait_for_license(session.invoice_id, timeout_s=30*60)
```

```rust
// Rust
use licensing_client::{Client, StartPurchaseOptions};
let client = Client::new("https://licensing.example.com")?;
let tiers = client.list_public_policies(PRODUCT_SLUG).await?;
// render tiers.policies; user picks "pro"
let session = client.start_purchase(PRODUCT_SLUG, &StartPurchaseOptions {
    policy_slug: Some("pro"),
    buyer_email: Some("buyer@example.com"),
    ..Default::default()
}).await?;
// open session.checkout_url; poll on settle
```

```go
// Go
client := keysat.NewClient("https://licensing.example.com", nil)
tiers, _ := client.ListPublicPolicies(ctx, PRODUCT_SLUG)
// render tiers.Policies; user picks "pro"
session, _ := client.StartPurchase(ctx, PRODUCT_SLUG, keysat.StartPurchaseOptions{
    PolicySlug: "pro",
    BuyerEmail: "buyer@example.com",
})
// open session.CheckoutURL; poll on settle
```

### Common mistakes

- **Hardcoding policy slugs in the client.** The whole point of
  `listPublicPolicies` is that the operator owns the tier shape. If
  you ship a build that only knows about Core and Pro, and the
  operator adds a "Patron" tier next month, the picker is silently
  stale. Render the picker off the live API response.
- **Splitting a product into multiple products.** Don't. Different
  tiers of the same product share the product slug and differ only on
  the policy slug. Splitting breaks `validate()` calls from clients
  that expect one canonical `productSlug`. The whole tier system is
  built on the assumption of one product, many policies.
- **Using discount codes as a tier discriminator.** `code` is for
  promos and referral discounts. It can't change which tier the buyer
  ends up on. Use `policySlug`.
- **Forgetting `policySlug` and assuming the right tier.** With
  `policySlug` omitted, the daemon picks the policy slugged "default"
  (if any), else the first active one. On a Core/Pro setup where
  Core happens to be alphabetically first or named "default", every
  buyer who hits your in-app upgrade flow without a `policySlug` ends
  up on Core regardless of what they clicked. Always pass the slug
  the buyer chose.
- **Copying the price from your hardcoded UI rather than the API.**
  Operators legitimately edit tier pricing in admin without warning;
  if you cache a price, you'll under- or over-charge buyers vs. what
  they actually pay. Render `policy.priceSats` directly from the
  current `listPublicPolicies` response.

### Architecture diagram

```
Buyer in your app
    │
    ▼
listPublicPolicies(slug)         ← public, no auth
    │
    │ returns [{slug, name, priceSats, entitlements, ...}, ...]
    ▼
your in-app tier picker UI       ← operator's branding
    │
    │ buyer clicks "Pro"
    ▼
startPurchase(slug, {policySlug: 'pro', buyerEmail, redirectUrl})
    │
    │ returns {checkoutUrl, invoiceId, ...}
    ▼
open checkoutUrl in browser      ← BTCPay or Zaprite
    │
    │ buyer pays
    ▼
operator's licensing service     ← webhook fires on settle
    │
    │ issues license with Pro entitlements + invoice.policy_id = 'pro'
    ▼
poll /v1/purchase/<id> OR webhook to your backend
    │
    │ returns license_key (signed string)
    ▼
write to /data/license.txt (or your chosen path)
    │
    ▼
checkLicense() reloads, app sees Pro entitlements
```

This is the same architecture Keysat itself uses for its own
self-licensing (cf section 17) and the same flow Recap implements
in their Recap app's activation screen.

---

## 12. UX patterns for revocation & errors

When `validate()` returns `ok: false`, the `reason` field tells you why:

| reason | What to show the user |
|---|---|
| `revoked` | "This license has been revoked by the seller. Contact support." |
| `fingerprint_mismatch` | "This license is already active on another computer." |
| `not_found` | "License key not recognized. Did you copy it correctly?" |
| `bad_signature` | "This license appears tampered. Contact support." |
| `product_mismatch` | "This license is for a different product." |
| `expired` | "Your license expired on <date>. Renew at <url>." |

Customize the copy for your tone, but show **distinct** messages — they
mean very different things to the user.

---

## 13. Migrating from a non-Bitcoin licensing scheme (Gumroad, Stripe, etc.)

If you already sell licenses through a non-Bitcoin system, you don't
have to do a flag-day migration. Two-phase plan:

**Phase 1: dual-stack.** Your app accepts BOTH old-format keys and
`LIC1-…` keys. They look different, so detection is trivial:

```ts
const isKeysat = raw.startsWith('LIC1-')
```

Honor old keys via your old verification path, new keys via the Keysat
SDK. Both unlock the same features. Existing customers see no change.

**Phase 2: cutover.** When you're ready to retire the old system,
issue fresh `LIC1-` keys for existing customers via the operator's
admin "Issue license manually" action and email them with a one-line
"here's your new key" note. Mark the old format deprecated; don't
break it for some grace period.

For free → paid migrations, use **free-license discount codes**: the
operator creates a code with `kind: free_license, max_uses: <your existing
user count>`, you put a "Redeem your existing-user code" button in your
app's first-launch screen, and existing users redeem once and never see
the prompt again.

---

## 14. Worked example: minimal Express server

A complete pattern for an Express + JS app. Copy-paste, replace
`MYAPP`, the slug, and the issuer PEM, and you have a working integration.

```js
// server/license.js
const fs = require('node:fs')
const path = require('node:path')
const { Verifier, PublicKey } = require('@keysat/licensing-client')

const PRODUCT_SLUG = 'myapp'
const LICENSE_PATH = process.env.MYAPP_LICENSE_KEY_PATH || '/data/license.txt'
const ISSUER_PEM = fs.readFileSync(
  path.join(__dirname, '..', 'assets', 'issuer.pub'),
  'utf8'
)
const verifier = new Verifier(PublicKey.fromPem(ISSUER_PEM))

function readKey() {
  if (process.env.MYAPP_LICENSE_KEY) return process.env.MYAPP_LICENSE_KEY.trim()
  try { return fs.readFileSync(LICENSE_PATH, 'utf8').trim() } catch { return null }
}

function checkLicense() {
  const raw = readKey()
  if (!raw) return { state: 'unlicensed', entitlements: new Set() }
  try {
    const ok = verifier.verify(raw)
    return {
      state: 'licensed',
      licenseId: ok.payload.licenseId,
      entitlements: new Set(ok.payload.entitlements || []),
      expiresAt: ok.payload.expiresAt
        ? new Date(ok.payload.expiresAt * 1000)
        : null,
      isTrial: !!(ok.payload.flags & 1),
    }
  } catch (e) {
    return { state: 'invalid', reason: e.message, entitlements: new Set() }
  }
}

module.exports = { checkLicense, LICENSE_PATH }
```

```js
// server/index.js
const express = require('express')
const { checkLicense, LICENSE_PATH } = require('./license')

const app = express()
const LIC = checkLicense()
console.log(`[license] state=${LIC.state} entitlements=[${[...LIC.entitlements].join(',')}]`)

if (LIC.state === 'invalid') {
  console.warn(`[license] invalid: ${LIC.reason} — running unlicensed`)
}

// Free for everyone
app.get('/api/healthz', (_, res) => res.json({ ok: true }))

// Free tier: limited basic feature
app.post('/api/basic', (req, res) => {
  res.json({ ok: true, result: 'basic feature' })
})

// Paid feature — gated on the `export` entitlement
app.post('/api/export', (req, res) => {
  if (!LIC.entitlements.has('export')) {
    return res.status(402).json({
      error: 'feature_not_in_tier',
      message: 'Export requires a paid license. See <upgrade_url>.',
      license_path: LICENSE_PATH,
    })
  }
  res.json({ ok: true, result: 'paid export' })
})

// Buyer-facing license status (so the frontend can show "licensed" badge
// and construct the buy URL without hard-coding it).
const KEYSAT_BASE_URL = 'https://licensing.example.com'  // operator's instance

app.get('/api/license-status', (_, res) => {
  res.json({
    state: LIC.state,
    reason: LIC.reason || null,
    licenseId: LIC.licenseId || null,
    entitlements: [...LIC.entitlements],
    expiresAt: LIC.expiresAt || null,
    isTrial: !!LIC.isTrial,
    productSlug: PRODUCT_SLUG,
    keysatBaseUrl: KEYSAT_BASE_URL,
  })
})

app.listen(process.env.PORT || 8080)
```

**That's a complete integration.** ~75 lines. Replace the slug, the
PEM file, and the entitlement names with what your operator chose, and
ship it.

---

## 15. Common mistakes

- **Embedding the wrong key.** The PEM you embed is the **public** key
  (from `GET /v1/issuer/public-key`). The private key never leaves the
  operator's Keysat. If you accidentally ship a private key, every
  attacker can mint licenses.
- **Hard-failing on `validate()` errors.** If your app refuses to boot
  when validation throws, you've gated on the operator's server uptime.
  Always treat network errors as "status unknown" and fall back to the
  offline check.
- **Calling `validate()` in a hot loop.** Once at startup + once per
  hour is plenty.
- **Slug mismatch.** A license issued for slug `foo` won't validate
  against slug `bar`. Typos in the slug constant cause "license valid
  but my code rejects it" head-scratchers. Read the slug from a
  single constant.
- **Logging the full license key.** It's a bearer credential — log
  the `license_id` instead.
- **Refusing to start without a license.** Boot in unlicensed mode and
  let the user keep using whatever's free-tier. Much better UX than
  exit-on-startup.
- **Forgetting to `COPY` the new license module into the container.**
  If your Dockerfile lists individual server files explicitly, adding
  `server/license.js` requires its own `COPY` line. Build succeeds,
  container starts, then crashes at startup with `Cannot find module
  './license.js'`. See §7e for the full Docker checklist.
- **Letting the SDK ship without a built `dist/`.** Git installs of
  the Keysat client *only* work if the package has a `prepare` script
  that builds on install (or commits its `dist/` directory). Without
  that, the install succeeds but the package is empty. If you publish
  to npm, this isn't a problem — `prepublishOnly` builds `dist/` for
  you. If you only host on GitHub, ensure `prepare` is wired.
- **Using `github:user/repo` shorthand instead of `git+https://...`.**
  The shorthand often resolves to SSH on machines with a GitHub key,
  which then breaks every hermetic build downstream. Always use the
  explicit `git+https://github.com/...` form, and double-check the
  `resolved:` field in your `package-lock.json` after switching — npm
  caches the previous resolution and may keep an SSH URL in the lock
  even after you change the spec.
- **Skipping the frontend half of hard-gate Flavor 2.** A server-only
  integration boots happily but every request 402s, which the
  unlicensed user experiences as a broken app rather than a clear
  "activate to continue" screen. See §7f for the framework-agnostic
  pattern.

---

## 16. Testing the integration

1. Get a real license to test against. Easiest: ask the operator to
   issue you one manually from their admin UI's "Manually issue a
   license" form (Licenses tab). Or, if they've created a `free_license`
   discount code, redeem it: `curl -X POST https://licensing.example.com/v1/redeem -H 'content-type: application/json' -d '{"product":"<slug>","code":"<code>"}'` — the response includes a `license_key`.
2. Save the key to `/data/license.txt` (or wherever you read from).
3. Restart your app.
4. Look for a startup log line: `[license] state=licensed entitlements=[…]`.
5. Hit a paid endpoint — should succeed.
6. Hit a paid endpoint after deleting the license file — should return
   402.
7. Tamper with one character of the key — should log `state=invalid`.
8. (Online) Have the operator revoke the license; on next online check,
   reason should be `revoked`.

---

## 17. Reference: Keysat dogfoods this same pattern

The Keysat daemon itself uses this exact integration to license itself.
[`license_self.rs`](./licensing-service-startos/licensing-service/src/license_self.rs)
in the Keysat repo:

- Embeds the master public key as `TRUST_ROOT_PUBKEY_PEM`.
- Reads the license from `/data/keysat-license.txt` at boot.
- Verifies via the same `parse_key + verify_payload` machinery the SDK
  uses.
- Exposes `state.self_tier.entitlements` to the rest of the daemon.
- Other handlers gate features on entitlements (e.g., `unlimited_products`,
  `recurring_billing`) — see [`tier.rs`](./licensing-service-startos/licensing-service/src/api/tier.rs)
  for the canonical gate-helper pattern.

If you want a working precedent to copy, that's the cleanest one in the
codebase. The pattern is identical to what your app should do.
