# Developer integration guide

This guide is for developers who want their software to validate against a licensing-service instance. It doesn't matter whether your software is a Start9 package, a desktop app, or a server — the flow is the same.

## Core idea: two-phase validation

Licensing-service separates verification into two concerns:

1. **Signature verification** (offline, fast, deterministic) — prove the key was actually issued by the server. Needs only the server's Ed25519 public key, which you ship with your client.
2. **Revocation check** (online, authoritative) — confirm the server hasn't revoked the license. Requires a network call.

For most software, you should do both on startup, then **cache the revocation result** for some period (hours to a day) and fall back to the cached result if the server is briefly unreachable. That way:

- A bad or forged key is rejected instantly, without a network call.
- A legitimately paying user isn't locked out if the licensing server has a 10-minute hiccup.
- A revoked key is detected within your cache window.

## Bundling the public key

When you set up your licensing-service instance, fetch the public key once:

```bash
curl -s https://license.example.com/v1/pubkey | jq -r .public_key_pem
```

Commit the resulting PEM into your client source tree. **Do not fetch it dynamically at runtime** — that would let an attacker who compromises your licensing server swap the key and re-sign forged licenses retroactively. A pinned public key is the whole point.

> **Official SDKs exist — use them first.** Four wire-compatible client SDKs
> are published: TypeScript (`@keysat/licensing-client` on npm), Rust
> (`keysat-licensing-client` on crates.io), Python (`keysat-licensing-client`
> on PyPI), and Go (`github.com/keysat-xyz/keysat-client-go`). Install commands
> are in the main README. The by-hand reference implementations below are a
> fallback for languages without an SDK, or for understanding exactly what the
> SDKs do under the hood.

## Reference integration in Rust

This is what a Start9 package written in Rust might look like if you verify by
hand instead of using the Rust SDK:

```rust
use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use ed25519_dalek::pkcs8::DecodePublicKey;
use data_encoding::BASE32_NOPAD;

// Pinned at compile time from the licensing server's /v1/pubkey output.
const SERVER_PUBLIC_KEY_PEM: &str = r#"
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA...your-public-key...
-----END PUBLIC KEY-----
"#;

const LICENSING_URL: &str = "https://license.example.com";
const PRODUCT_SLUG: &str = "my-app";

pub struct LicenseCheck {
    pub license_id: String,
    pub product_id: String,
}

pub fn offline_verify(license_key: &str) -> Result<()> {
    let vk = VerifyingKey::from_public_key_pem(SERVER_PUBLIC_KEY_PEM)
        .context("bundled public key is invalid")?;

    let mut parts = license_key.trim().splitn(3, '-');
    let prefix = parts.next().context("empty key")?;
    anyhow::ensure!(prefix == "LIC1", "unknown key prefix");
    let payload_b32 = parts.next().context("no payload")?;
    let sig_b32 = parts.next().context("no signature")?;

    let payload = BASE32_NOPAD.decode(payload_b32.to_ascii_uppercase().as_bytes())?;
    let sig_bytes = BASE32_NOPAD.decode(sig_b32.to_ascii_uppercase().as_bytes())?;
    let sig_array: [u8; 64] = sig_bytes.as_slice().try_into()
        .context("signature length != 64")?;
    let sig = Signature::from_bytes(&sig_array);

    vk.verify(&payload, &sig).context("signature invalid")?;
    Ok(())
}

pub async fn validate_online(
    license_key: &str,
    fingerprint: &str,
) -> Result<LicenseCheck> {
    #[derive(serde::Deserialize)]
    struct Resp {
        ok: bool,
        reason: Option<String>,
        license_id: Option<String>,
        product_id: Option<String>,
    }

    let resp: Resp = reqwest::Client::new()
        .post(format!("{LICENSING_URL}/v1/validate"))
        .json(&serde_json::json!({
            "key": license_key,
            "product_slug": PRODUCT_SLUG,
            "fingerprint": fingerprint,
        }))
        .send()
        .await?
        .json()
        .await?;

    if !resp.ok {
        anyhow::bail!("license rejected: {}", resp.reason.unwrap_or_default());
    }
    Ok(LicenseCheck {
        license_id: resp.license_id.unwrap(),
        product_id: resp.product_id.unwrap(),
    })
}
```

## Reference integration in TypeScript

```ts
import { webcrypto } from "node:crypto";

const SERVER_PUBLIC_KEY_PEM = `
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA...your-public-key...
-----END PUBLIC KEY-----
`;
const LICENSING_URL = "https://license.example.com";
const PRODUCT_SLUG = "my-app";

function base32NoPadDecode(s: string): Uint8Array {
  const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const out: number[] = [];
  let bits = 0, value = 0;
  for (const c of s.toUpperCase()) {
    const idx = ALPHABET.indexOf(c);
    if (idx < 0) throw new Error("bad base32 char: " + c);
    value = (value << 5) | idx;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((value >> bits) & 0xff);
    }
  }
  return new Uint8Array(out);
}

async function importPubKey(): Promise<CryptoKey> {
  const pem = SERVER_PUBLIC_KEY_PEM
    .replace(/-----(BEGIN|END) PUBLIC KEY-----/g, "")
    .replace(/\s+/g, "");
  const der = Uint8Array.from(Buffer.from(pem, "base64"));
  return webcrypto.subtle.importKey("spki", der, { name: "Ed25519" }, false, ["verify"]);
}

export async function offlineVerify(key: string): Promise<void> {
  const [prefix, payloadB32, sigB32] = key.trim().split("-");
  if (prefix !== "LIC1") throw new Error("bad prefix");
  const payload = base32NoPadDecode(payloadB32);
  const sig = base32NoPadDecode(sigB32);
  const pk = await importPubKey();
  const ok = await webcrypto.subtle.verify("Ed25519", pk, sig, payload);
  if (!ok) throw new Error("signature invalid");
}

export async function validateOnline(key: string, fingerprint: string) {
  const r = await fetch(`${LICENSING_URL}/v1/validate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key, product_slug: PRODUCT_SLUG, fingerprint }),
  });
  const body = await r.json();
  if (!body.ok) throw new Error(`license rejected: ${body.reason}`);
  return body;
}
```

## Graceful degradation pattern

```
on startup:
  key = read_license_from_storage()
  if key is None:
      prompt_user_for_license_or_start_trial()
      return

  try offline_verify(key)              # instant; fail closed on bad signature
  except BadSignature:
      mark_installation_unlicensed()
      return

  try online_validate(key, fingerprint)
  except NetworkError:
      cached = read_cache()
      if cached is valid and < 7 days old:
          proceed()
      else:
          warn_user("licensing server unreachable for > 7 days")
          proceed()   # or refuse, if you prefer strict
  except Rejected(reason):
      handle_rejection(reason)

  on every N hours in background:
      re-run online_validate, refresh cache
```

Choosing the cache TTL is a business decision: long TTL = better uptime resilience, slower revocation propagation. A day to a week covers most sane cases.

## Fingerprint strategy

A fingerprint is any string that uniquely identifies an installation. Common choices, roughly from stable to less stable:

- A random 256-bit value you generate and persist in your app's data directory on first run. **Recommended** — stable across reboots, you control it, doesn't leak anything about the host.
- On Start9: the service's `TOR_ADDRESS` env var, hashed.
- Machine UUID from `/etc/machine-id` on Linux. Leaks a real identifier but is available without any state.
- Combination of MAC + hostname — avoid; user-visible and changes on network moves.

Whatever you pick, hash it before sending if you want to avoid exposing the underlying identifier in network traffic.

## Reasoning about failure modes

| Scenario                                 | What happens                                               |
|------------------------------------------|------------------------------------------------------------|
| Licensing server down, user has valid key | Your software uses cached result and keeps working.       |
| Licensing server down, first-ever startup | Offline verification passes; online validation fails; you decide whether to proceed or block. |
| Forged key                                | Offline verification rejects instantly, no network call. |
| Valid key but revoked                     | Online validation returns `reason: "revoked"`; block or downgrade. |
| Valid key but user swaps hardware         | Online validation returns `fingerprint_mismatch`; user contacts you to transfer. |
| Network censorship in user's region       | Consider shipping a Tor client so they can reach your `.onion`. |

## Tor / `.onion` support

Since licensing-service runs on Start9, it automatically gets a Tor `.onion` address. If you ship a Tor transport in your client, you get censorship-resistant validation for free, which is particularly valuable given the whole stack is Bitcoin-native and privacy-adjacent.
