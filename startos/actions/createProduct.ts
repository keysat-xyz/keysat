// Action: create a new product for sale.
//
// Hits the service's admin API through localhost using the in-store admin
// key. No need for the operator to touch curl or handle tokens.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  slug: {
    type: 'text',
    name: 'Slug',
    description: 'URL-safe short name, e.g., "my-app". Used in product links.',
    required: true,
    default: null,
    patterns: [{ regex: '^[a-z0-9-]{2,40}$', description: 'lowercase letters, digits, and dashes' }],
  },
  name: {
    type: 'text',
    name: 'Name',
    description: 'Display name shown to buyers.',
    required: true,
    default: null,
  },
  description: {
    type: 'textarea',
    name: 'Description',
    description: 'Public description of what the buyer is getting.',
    required: false,
    default: null,
  },
  price_sats: {
    type: 'number',
    name: 'Price (sats)',
    description: 'Price per license in satoshis. 100,000,000 sats = 1 BTC.',
    required: true,
    default: null,
    min: 1,
    max: null,
    integer: true,
  },
})

export const createProduct = sdk.Action.withInput(
  'createProduct',
  async ({ effects }) => ({
    name: 'Create product',
    description: 'Add a new product that can be purchased through this service.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Products',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(LICENSING_URL, store.admin_api_key, '/v1/admin/products', {
      method: 'POST',
      body: JSON.stringify({
        slug: formInput.slug,
        name: formInput.name,
        description: formInput.description ?? '',
        price_sats: formInput.price_sats,
        metadata: {},
      }),
    })
    if (!resp.ok) {
      throw new Error(`Create product failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = await resp.json()
    return {
      message:
        `Created product '${body.slug}' (id ${body.id}).\n` +
        `Priced at ${body.price_sats} sats.\n\n` +
        `Buyers can purchase by POSTing to your Keysat URL:\n` +
        `<your Keysat URL>/v1/purchase with body: {"product":"${body.slug}"}`,
    }
  },
)
