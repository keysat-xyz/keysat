// Action: reveal the auto-generated admin API key.
//
// The operator rarely needs this — every other action in StartOS already
// carries the key for them — but it's useful if they want to script against
// the admin HTTP API directly.
//
// The BTCPay webhook secret used to live in the StartOS store; it now lives
// inside the daemon's own SQLite database, generated automatically during
// the "Connect BTCPay" authorize flow. Operators don't need to know it.
//
// SDK 0.4.0 shape: `Action.withoutInput(id, metadata, run)` — the run fn is
// the third positional arg, not a chained `.withoutRunner(...)` method.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'

export const showCredentials = sdk.Action.withoutInput(
  'show-credentials',
  async () => ({
    name: 'Show admin API key',
    description:
      'Display the auto-generated admin API key. Treat it like a password — ' +
      'anyone with this key can mint and revoke licenses on this server.',
    warning:
      'Anyone with this value has full control of your Keysat server. ' +
      'Do not share it.',
    allowedStatuses: 'any',
    group: 'Credentials',
    visibility: 'enabled',
  }),
  async () => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    return {
      version: '1',
      title: 'Admin API key',
      message:
        `Used as 'Authorization: Bearer <key>' against /v1/admin/*. All ` +
        `StartOS actions already supply this for you — only export it if ` +
        `you intend to script against the admin API from outside the box.`,
      result: {
        type: 'single',
        value: storeData.admin_api_key,
        copyable: true,
        qr: true,
        masked: true,
      },
    }
  },
)
