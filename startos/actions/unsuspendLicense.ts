// Action: clear a previously-applied suspension.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  license_id: {
    type: 'text',
    name: 'License ID',
    description: 'UUID of the suspended license to re-enable.',
    required: true,
    default: null,
  },
})

export const unsuspendLicense = sdk.Action.withInput(
  'unsuspendLicense',
  async ({ effects }) => ({
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
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      `/v1/admin/licenses/${encodeURIComponent(formInput.license_id)}/unsuspend`,
      { method: 'POST', body: JSON.stringify({}) },
    )
    if (!resp.ok) {
      throw new Error(`Unsuspend failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return { message: `Unsuspended license ${formInput.license_id}.` }
  },
)
