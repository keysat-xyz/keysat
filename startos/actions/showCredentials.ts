// Action: reveal the auto-generated admin API key.
//
// The operator needs this on first install to sign into the admin web UI
// (until they set a web UI password); afterward it's mainly for scripting
// the admin HTTP API directly, since every other StartOS action already
// carries the key for them.
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
        `This is your admin API key — the 'Authorization: Bearer <key>' ` +
        `credential for /v1/admin/*. Use it to sign into the admin web UI on ` +
        `first install (until you set a web UI password). Every StartOS action ` +
        `already supplies it for you, so you only need to export it to script ` +
        `the admin API yourself.`,
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
