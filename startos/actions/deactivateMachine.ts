// Action: force-kick an install off a license.
//
// The buyer's copy on that device will fail its next online validation
// with `not_activated`, freeing up a seat for another install.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  machine_id: {
    type: 'text',
    name: 'Machine ID',
    description: 'UUID of the machine to deactivate. Find via list-machines.',
    required: true,
    default: null,
  },
  reason: {
    type: 'text',
    name: 'Reason',
    description: 'Stored for audit. E.g., "laptop stolen", "support request".',
    required: false,
    default: null,
  },
})

export const deactivateMachine = sdk.Action.withInput(
  'deactivateMachine',
  async ({ effects }) => ({
    name: 'Deactivate machine',
    description:
      'Force an install off a license. Frees up a seat and causes that ' +
      "install's next online validation to fail.",
    warning:
      'The affected client may continue running from cache until its grace ' +
      'window expires.',
    allowedStatuses: 'only-running',
    group: 'Machines',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      `/v1/admin/machines/${encodeURIComponent(formInput.machine_id)}/deactivate`,
      {
        method: 'POST',
        body: JSON.stringify({ reason: formInput.reason ?? '' }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Deactivate failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return { message: `Deactivated machine ${formInput.machine_id}.` }
  },
)
