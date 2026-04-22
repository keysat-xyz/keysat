// Action: manually issue a license for a product (comp, press, dev).

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  product_slug: {
    type: 'text',
    name: 'Product slug',
    description: 'Which product to issue a license for.',
    required: true,
    default: null,
  },
  note: {
    type: 'text',
    name: 'Note (optional)',
    description: 'Audit trail — e.g., "comp for @alice", "press key".',
    required: false,
    default: null,
  },
})

export const issueLicense = sdk.Action.withInput(
  'issueLicense',
  async ({ effects }) => ({
    name: 'Issue license manually',
    description: 'Generate a license key outside the purchase flow. Useful for comps and press.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Licenses',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(LICENSING_URL, store.admin_api_key, '/v1/admin/licenses', {
      method: 'POST',
      body: JSON.stringify({
        product_slug: formInput.product_slug,
        note: formInput.note ?? null,
      }),
    })
    if (!resp.ok) {
      throw new Error(`Issue failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = await resp.json()
    return {
      message:
        `License issued.\nID: ${body.license_id}\n\n` +
        `Key (give this to the recipient):\n${body.license_key}`,
    }
  },
)
