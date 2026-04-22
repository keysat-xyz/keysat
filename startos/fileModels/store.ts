// Package-local persistent state. This is separate from the SQLite database
// inside the container — it's metadata the StartOS wrapper needs to remember
// between service starts (e.g., the generated admin API key so we don't
// regenerate it on every restart).
//
// StartOS persists this JSON through upgrades and backs it up automatically.

import { matches } from '@start9labs/start-sdk'

const { arr, num, obj, oneOf, literal, string } = matches

export const storeShape = obj({
  // Admin API key for /v1/admin/* endpoints. Auto-generated on first init.
  admin_api_key: string,
  // Shared webhook secret configured on both sides (BTCPay + our service).
  btcpay_webhook_secret: string,
  // Operator display name shown on the service homepage.
  operator_name: string,
  // Tracks which version's init has already been applied.
  schema_version: num,
})

export type Store = typeof storeShape._TYPE

export const store = {
  shape: storeShape,
  // Defaults. Populated for real during init.
  path: 'store.json' as const,
}
