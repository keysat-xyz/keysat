// Action: create a license policy (reusable template) for a product.
//
// Policies let the operator capture "when someone buys this product, issue a
// license with these defaults" (duration, grace period, entitlements, seat
// cap, trial flag). A policy slugged `default` is used automatically by the
// normal purchase flow.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  product_slug: Value.text({
    name: 'Product slug',
    description: 'The product this policy applies to.',
    required: true,
    default: null,
  }),
  slug: Value.text({
    name: 'Policy slug',
    description:
      'URL-safe name, e.g., "default", "annual", "trial". ' +
      'Use "default" for the one consumed by the public purchase flow.',
    required: true,
    default: null,
    patterns: [
      { regex: '^[a-z0-9-]{2,40}$', description: 'lowercase letters, digits, and dashes' },
    ],
  }),
  name: Value.text({
    name: 'Display name',
    description: 'Shown in admin listings. E.g., "Annual subscription".',
    required: true,
    default: null,
  }),
  duration_seconds: Value.number({
    name: 'Duration (seconds)',
    description: '0 = perpetual. 31536000 = one year. 7776000 = 90 days.',
    required: true,
    default: 0,
    min: 0,
    max: null,
    integer: true,
  }),
  grace_seconds: Value.number({
    name: 'Grace period (seconds)',
    description:
      'After expiry, how long a cached validation remains honoured ' +
      'before the client must reach the server again. 0 = no grace.',
    required: true,
    default: 0,
    min: 0,
    max: null,
    integer: true,
  }),
  max_machines: Value.number({
    name: 'Max machines',
    description: '0 = unlimited, 1 = single-seat, n>1 = multi-seat cap.',
    required: true,
    default: 1,
    min: 0,
    max: null,
    integer: true,
  }),
  is_trial: Value.toggle({
    name: 'Trial policy',
    description: 'Mark issued keys as trial (sets the TRIAL flag in the payload).',
    default: false,
  }),
  entitlements: Value.text({
    name: 'Entitlements',
    description:
      'Comma-separated list of feature slugs embedded in the license key. ' +
      'E.g., "pro,multi-device". Leave blank for none.',
    required: false,
    default: null,
  }),
  price_sats_override: Value.number({
    name: 'Price override (sats, optional)',
    description:
      "Override the product's default price for licenses issued under this " +
      'policy. Leave at -1 to use the product price.',
    required: true,
    default: -1,
    min: -1,
    max: null,
    integer: true,
  }),
})

export const createPolicy = sdk.Action.withInput(
  'create-policy',
  async () => ({
    name: 'Create policy',
    description:
      'Add a reusable license template to a product. The public purchase ' +
      'flow picks up the policy slugged "default"; other policies are used ' +
      'by the admin "Issue license manually" action.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Policies',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    const entitlements = (formInput.entitlements ?? '')
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
    const body: Record<string, unknown> = {
      product_slug: formInput.product_slug,
      name: formInput.name,
      slug: formInput.slug,
      duration_seconds: formInput.duration_seconds,
      grace_seconds: formInput.grace_seconds,
      max_machines: formInput.max_machines,
      is_trial: formInput.is_trial,
      entitlements,
      metadata: {},
    }
    if (formInput.price_sats_override >= 0) {
      body.price_sats_override = formInput.price_sats_override
    }

    const resp = await adminCall(LICENSING_URL, storeData.admin_api_key, '/v1/admin/policies', {
      method: 'POST',
      body: JSON.stringify(body),
    })
    if (!resp.ok) {
      throw new Error(`Create policy failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const policy = (await resp.json()) as { id: string; slug: string; name: string }
    return {
      version: '1',
      title: 'Policy created',
      message:
        `Created policy '${policy.slug}' (id ${policy.id}).\n` +
        (formInput.slug === 'default'
          ? 'Because the slug is "default", this policy will be used by the public purchase flow.'
          : 'Use this slug when calling "Issue license manually".'),
      result: null,
    }
  },
)
