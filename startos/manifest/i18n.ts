// Human-readable description strings, separated so they can be translated
// later. Only English is filled in here; add more locales as needed.

export const short = {
  en_US: 'Bitcoin-native self-hosted licensing service for software creators.',
}

export const long = {
  en_US: `Keysat is a Bitcoin-native, self-hosted licensing service for
software creators. Sell licenses to your own software products with payments
via BTCPay Server (Bitcoin / Lightning) or Zaprite (Bitcoin + cards). Every
instance runs on the operator's own StartOS — there is no central authority.
The service issues Ed25519-signed license keys that downstream software can
verify offline, with optional expiry, entitlements, fingerprint binding, and
per-seat activation caps. Supports multiple products per instance, recurring
subscriptions, in-place tier upgrades, and closed-source, open-core, and
open-source distribution models.`,
}
