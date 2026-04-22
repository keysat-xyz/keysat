// First-boot initialization.
//
// On fresh install:
//   - Generate an admin API key (stored in the StartOS store; user can
//     retrieve it via an action if they need to script against the API).
//
// The BTCPay webhook secret is no longer stored here — the daemon generates
// and persists it in its own DB during the one-click "Connect BTCPay" flow.
// The field is kept in the store shape for backward compatibility with
// installs made before v0.1.0; it is not used.
//
// On subsequent boots this is a no-op (keys already exist).

import { sdk } from '../sdk'
import { generateSecret } from '../utils'

export const initFn = sdk.setupOnInit(async ({ effects }) => {
  const current = await sdk.store.getOwn(effects, sdk.StorePath).const()

  if (!current || current.schema_version === 0 || current.schema_version === undefined) {
    await sdk.store.setOwn(effects, sdk.StorePath, {
      admin_api_key: current?.admin_api_key || generateSecret(32),
      // Kept in the shape for backcompat; no longer authoritative.
      btcpay_webhook_secret: current?.btcpay_webhook_secret || '',
      operator_name: current?.operator_name || '',
      schema_version: 1,
    })
  }
})

export const uninitFn = sdk.setupOnUninit(async ({ effects }) => {
  // Nothing to tear down at the StartOS level — the DB volume is handled by
  // StartOS directly when the package is uninstalled.
})
