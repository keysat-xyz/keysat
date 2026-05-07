// Action: manually issue a license for a product (comp, press, dev).

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  product_slug: Value.text({
    name: 'Product slug',
    description: 'Which product to issue a license for.',
    required: true,
    default: null,
  }),
  note: Value.text({
    name: 'Note (optional)',
    description: 'Audit trail — e.g., "comp for @alice", "press key".',
    required: false,
    default: null,
  }),
})

export const issueLicense = sdk.Action.withInput(
  'issue-license',
  async () => ({
    name: 'Issue license manually',
    description: 'Generate a license key outside the purchase flow. Useful for comps and press.',
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
    const resp = await adminCall(LICENSING_URL, storeData.admin_api_key, '/v1/admin/licenses', {
      method: 'POST',
      body: JSON.stringify({
        product_slug: formInput.product_slug,
        note: formInput.note ?? null,
      }),
    })
    if (!resp.ok) {
      throw new Error(`Issue failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as { license_id: string; license_key: string }
    return {
      version: '1',
      title: 'License issued',
      message: `License ID: ${body.license_id}\n\nGive this key to the recipient.`,
      result: {
        type: 'single',
        value: body.license_key,
        copyable: true,
        qr: true,
        masked: true,
      },
    }
  },
)
