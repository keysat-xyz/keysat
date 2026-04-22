// Action: reveal the auto-generated admin API key.
//
// The operator rarely needs this — every other action in StartOS already
// carries the key for them — but it's useful if they want to script against
// the admin HTTP API directly.
//
// The BTCPay webhook secret used to live in the StartOS store; it now lives
// inside the daemon's own SQLite database, generated automatically during
// the "Connect BTCPay" authorize flow. Operators don't need to know it.

import { sdk } from '../sdk'

export const showCredentials = sdk.Action.withoutInput(
  'showCredentials',
  async ({ effects }) => ({
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
).withoutRunner(async ({ effects }) => {
  const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
  return {
    message:
      `Admin API key:\n${store.admin_api_key}\n\n` +
      `Used as 'Authorization: Bearer <key>' against /v1/admin/*. All ` +
      `StartOS actions already supply this for you — only export it if ` +
      `you intend to script against the admin API from outside the box.`,
  }
})
