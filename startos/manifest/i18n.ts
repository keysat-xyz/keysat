// Human-readable description strings, separated so they can be translated
// later. Only English is filled in here; add more locales as needed.

export const short = {
  en_US: 'Keysat — self-hosted Bitcoin-paid software license server.',
}

export const long = {
  en_US: `Keysat lets you sell licenses to your own software products using
Bitcoin payments via BTCPay Server. Every instance runs on the operator's own
StartOS — there is no central authority. The service issues Ed25519-signed
license keys that downstream software can verify offline, with optional
expiry, entitlements, fingerprint binding, and per-seat activation caps.
Supports multiple products per instance and closed-source, open-core, and
open-source distribution models.`,
}
