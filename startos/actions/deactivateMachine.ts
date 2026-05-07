// Action: force-kick an install off a license.
//
// The buyer's copy on that device will fail its next online validation
// with `not_activated`, freeing up a seat for another install.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  machine_id: Value.text({
    name: 'Machine ID',
    description: 'UUID of the machine to deactivate. Find via list-machines.',
    required: true,
    default: null,
  }),
  reason: Value.text({
    name: 'Reason',
    description: 'Stored for audit. E.g., "laptop stolen", "support request".',
    required: false,
    default: null,
  }),
})

export const deactivateMachine = sdk.Action.withInput(
  'deactivate-machine',
  async () => ({
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
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      `/v1/admin/machines/${encodeURIComponent(formInput.machine_id)}/deactivate`,
      {
        method: 'POST',
        body: JSON.stringify({ reason: formInput.reason ?? '' }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Deactivate failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return {
      version: '1',
      title: 'Machine deactivated',
      message: `Deactivated machine ${formInput.machine_id}.`,
      result: null,
    }
  },
)
