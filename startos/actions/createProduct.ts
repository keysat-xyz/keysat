// Action: create a new product for sale.
//
// Hits the service's admin API through localhost using the in-store admin
// key. No need for the operator to touch curl or handle tokens.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  slug: Value.text({
    name: 'Slug',
    description: 'URL-safe short name, e.g., "my-app". Used in product links.',
    required: true,
    default: null,
    patterns: [
      { regex: '^[a-z0-9-]{2,40}$', description: 'lowercase letters, digits, and dashes' },
    ],
  }),
  name: Value.text({
    name: 'Name',
    description: 'Display name shown to buyers.',
    required: true,
    default: null,
  }),
  description: Value.textarea({
    name: 'Description',
    description: 'Public description of what the buyer is getting.',
    required: false,
    default: null,
  }),
  price_sats: Value.number({
    name: 'Price (sats)',
    description: 'Price per license in satoshis. 100,000,000 sats = 1 BTC.',
    required: true,
    default: null,
    min: 1,
    max: null,
    integer: true,
  }),
})

export const createProduct = sdk.Action.withInput(
  'create-product',
  async () => ({
    name: 'Create product',
    description: 'Add a new product that can be purchased through this service.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Products',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(LICENSING_URL, storeData.admin_api_key, '/v1/admin/products', {
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
    const body = (await resp.json()) as { id: string; slug: string; price_sats: number }
    return {
      version: '1',
      title: 'Product created',
      message:
        `Created product '${body.slug}' (id ${body.id}).\n` +
        `Priced at ${body.price_sats} sats.\n\n` +
        `Buyers can purchase by POSTing to your Keysat URL:\n` +
        `<your Keysat URL>/v1/purchase with body: {"product":"${body.slug}"}`,
      result: null,
    }
  },
)
