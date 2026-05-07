// Action: revoke an existing license.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  license_id: Value.text({
    name: 'License ID',
    description: 'UUID of the license to revoke. Find via list-licenses action.',
    required: true,
    default: null,
  }),
  reason: Value.text({
    name: 'Reason',
    description: 'Stored for audit. E.g., "chargeback", "key leaked".',
    required: false,
    default: null,
  }),
})

export const revokeLicense = sdk.Action.withInput(
  'revoke-license',
  async () => ({
    name: 'Revoke license',
    description:
      'Mark a license as revoked (one-way; use "Suspend license" for a ' +
      'reversible lockout). The next time downstream software checks ' +
      'revocation, it will be denied.',
    warning:
      'Revocation takes effect on the next online validation. Clients with ' +
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
      `/v1/admin/licenses/${encodeURIComponent(formInput.license_id)}/revoke`,
      {
        method: 'POST',
        body: JSON.stringify({ reason: formInput.reason ?? 'admin revoke' }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Revoke failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return {
      version: '1',
      title: 'License revoked',
      message: `Revoked license ${formInput.license_id}.`,
      result: null,
    }
  },
)
