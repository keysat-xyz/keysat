// Action: suspend an existing license (reversible).
//
// Unlike revoke (which is one-way), suspend temporarily blocks validation
// and can be cleared with the "Unsuspend" action. Useful for payment
// disputes where the outcome isn't yet known.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  license_id: Value.text({
    name: 'License ID',
    description: 'UUID of the license to suspend. Find via search-licenses action.',
    required: true,
    default: null,
  }),
  reason: Value.text({
    name: 'Reason',
    description: 'Stored for audit. E.g., "payment dispute pending".',
    required: false,
    default: null,
  }),
})

export const suspendLicense = sdk.Action.withInput(
  'suspend-license',
  async () => ({
    name: 'Suspend license',
    description:
      'Temporarily disable a license. Validation calls will fail with a ' +
      '`suspended` status until you unsuspend. Use this for reversible ' +
      'situations (e.g., payment disputes) instead of revoke.',
    warning:
      'Suspension takes effect on the next online validation. Clients with ' +
      'cached results may continue running until their cache expires.',
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
      `/v1/admin/licenses/${encodeURIComponent(formInput.license_id)}/suspend`,
      {
        method: 'POST',
        body: JSON.stringify({ reason: formInput.reason ?? 'admin suspend' }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Suspend failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return {
      version: '1',
      title: 'License suspended',
      message: `Suspended license ${formInput.license_id}.`,
      result: null,
    }
  },
)
