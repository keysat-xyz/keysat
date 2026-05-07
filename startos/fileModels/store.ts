// Package-local persistent state. This is separate from the SQLite database
// inside the container — it's metadata the StartOS wrapper needs to remember
// between service starts (e.g., the generated admin API key so we don't
// regenerate it on every restart).
//
// StartOS persists this JSON through upgrades and backs it up automatically
// (the file lives alongside the package data dir).
//
// In 0.4.0.x we model this with `FileHelper.json` + a Zod schema. Consumers
// read via `store.read().once()` (fire-and-forget) or `store.read().const(effects)`
// (re-runs the calling context if the file changes), and write with
// `store.write(effects, data)` or `store.merge(effects, partial)`.

import { FileHelper } from '@start9labs/start-sdk'
import { z } from 'zod'

export const storeShape = z.object({
  // Admin API key for /v1/admin/* endpoints. Auto-generated on first init.
  admin_api_key: z.string(),
  // Shared webhook secret historically configured on both sides (BTCPay +
  // our service). Kept in the shape for backwards compatibility with
  // installs made before the one-click "Connect BTCPay" authorize flow; the
  // daemon now generates and persists its own webhook secret.
  btcpay_webhook_secret: z.string(),
  // Operator display name shown on the service homepage.
  operator_name: z.string(),
  // Tracks which version's init has already been applied.
  schema_version: z.number(),
})

export type Store = z.infer<typeof storeShape>

export const store = FileHelper.json('store.json', storeShape)
