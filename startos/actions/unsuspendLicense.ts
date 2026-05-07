// Action: clear a previously-applied suspension.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  license_id: Value.text({
    name: 'License ID',
    description: 'UUID of the suspended license to re-enable.',
    required: true,
    default: null,
  }),
})

export const unsuspendLicense = sdk.Action.withInput(
  'unsuspend-license',
  async () => ({
    name: 'Unsuspend license',
    description:
      'Lift a previous suspension. Validation will succeed again on the ' +
      'next call. Has no effect if the license is already active or if it ' +
      'has been revoked.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Licenses',
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
      `/v1/admin/licenses/${encodeURIComponent(formInput.license_id)}/unsuspend`,
      { method: 'POST', body: JSON.stringify({}) },
    )
    if (!resp.ok) {
      throw new Error(`Unsuspend failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return {
      version: '1',
      title: 'License unsuspended',
      message: `Unsuspended license ${formInput.license_id}.`,
      result: null,
    }
  },
)
