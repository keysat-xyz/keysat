// Action: create a discount / referral code.
//
// A code can be percentage-off or fixed-sats-off. It can target a specific
// product (or apply universally), have an optional expiry, an optional
// usage cap, and a free-form referrer label for tracking ("twitter-launch",
// "alice@example.com"). Codes are case-insensitive and normalized to
// uppercase on create.
//
// Buyers redeem codes by passing `?code=FOUNDERS50` to the public purchase
// flow. The discount is reserved atomically at purchase time and finalized
// when the BTCPay invoice settles.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  code: Value.text({
    name: 'Code',
    description:
      'The redeemable string. Case-insensitive (will be uppercased). ' +
      'ASCII letters, digits, "-", and "_" only. E.g., "FOUNDERS50".',
    required: true,
    default: null,
    patterns: [
      {
        regex: '^[A-Za-z0-9_-]{2,40}$',
        description:
          'letters, digits, dashes, underscores; 2 to 40 characters',
      },
    ],
  }),
  kind: Value.select({
    name: 'Discount kind',
    description:
      '"Percent off" reduces the price by N%. ' +
      '"Fixed sats off" subtracts a fixed number of sats. ' +
      '"Free license" issues a license outright with no payment, ' +
      'redeemed via the public /v1/redeem endpoint.',
    default: 'percent',
    values: {
      percent: 'Percent off',
      fixed_sats: 'Fixed sats off',
      free_license: 'Free license (no payment)',
    },
  }),
  amount: Value.number({
    name: 'Amount',
    description:
      'For percent: 1..=100 (integer percentage). E.g., 50 = 50% off. ' +
      'For fixed sats off: any positive integer (sats). ' +
      'For free license: ignored (set to 0). ' +
      'Note: percent is converted to basis points server-side.',
    required: true,
    default: 50,
    min: 0,
    max: null,
    integer: true,
  }),
  max_uses: Value.number({
    name: 'Max uses',
    description: '0 = unlimited. Otherwise, max number of redemptions.',
    required: true,
    default: 0,
    min: 0,
    max: null,
    integer: true,
  }),
  expires_at: Value.text({
    name: 'Expires at (ISO 8601)',
    description:
      'Optional cutoff date. RFC3339 / ISO 8601 UTC, e.g. ' +
      '"2026-12-31T23:59:59Z". Leave blank for no expiry.',
    required: false,
    default: null,
  }),
  product_slug: Value.text({
    name: 'Product slug (optional)',
    description:
      'Restrict the code to one specific product. Leave blank to apply ' +
      'the discount to any product.',
    required: false,
    default: null,
  }),
  policy_slug: Value.text({
    name: 'Policy slug (optional)',
    description:
      'Further restrict to a single policy of the chosen product. ' +
      'Requires "Product slug" to be set if used.',
    required: false,
    default: null,
  }),
  referrer_label: Value.text({
    name: 'Referrer / campaign label (optional)',
    description:
      'Free-form tracking string. E.g., "twitter-launch", ' +
      '"partner-alice@example.com", "podcast-XYZ-spring-2026". Shown in ' +
      'usage reports; never visible to buyers.',
    required: false,
    default: null,
  }),
  description: Value.textarea({
    name: 'Description (optional)',
    description: 'Internal note. E.g., "Founders rate, expires May 31."',
    required: false,
    default: null,
  }),
})

export const createDiscountCode = sdk.Action.withInput(
  'create-discount-code',
  async () => ({
    name: 'Create discount code',
    description:
      'Add a redeemable discount / referral code. Buyers append ' +
      '?code=YOUR_CODE to the purchase URL to apply it.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Discount codes',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    if (formInput.policy_slug && !formInput.product_slug) {
      throw new Error('Policy slug requires Product slug to also be set.')
    }

    // Convert UI percent (1..=100) to basis points (100..=10000).
    let amount = formInput.amount
    if (formInput.kind === 'percent') {
      if (amount < 1 || amount > 100) {
        throw new Error('Percent amount must be between 1 and 100.')
      }
      amount = amount * 100
    } else if (formInput.kind === 'fixed_sats') {
      if (amount < 1) {
        throw new Error('Fixed sats amount must be at least 1.')
      }
    } else if (formInput.kind === 'free_license') {
      // Amount is unused for free licenses; force to 0 so the server
      // accepts it.
      amount = 0
    }

    const body: Record<string, unknown> = {
      code: formInput.code,
      kind: formInput.kind,
      amount,
      description: formInput.description ?? '',
    }
    if (formInput.max_uses > 0) body.max_uses = formInput.max_uses
    if (formInput.expires_at) body.expires_at = formInput.expires_at
    if (formInput.product_slug) body.product_slug = formInput.product_slug
    if (formInput.policy_slug) body.policy_slug = formInput.policy_slug
    if (formInput.referrer_label) body.referrer_label = formInput.referrer_label

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/discount-codes',
      { method: 'POST', body: JSON.stringify(body) },
    )
    if (!resp.ok) {
      throw new Error(`Create discount code failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const code = (await resp.json()) as { id: string; code: string; kind: string; amount: number }
    const humanAmount =
      code.kind === 'percent'
        ? `${code.amount / 100}% off`
        : code.kind === 'fixed_sats'
          ? `${code.amount} sats off`
          : 'free license (no payment)'
    const redemptionHint =
      code.kind === 'free_license'
        ? `Buyers redeem this code via the public /v1/redeem endpoint or via ` +
          `the "Redeem free license" buyer-side action — they receive the key ` +
          `directly, no BTCPay invoice.`
        : `Buyers can redeem this code by appending ?code=${code.code} to the ` +
          `public purchase URL.`
    return {
      version: '1',
      title: 'Discount code created',
      message:
        `Created code "${code.code}" — ${humanAmount}.\n` +
        redemptionHint +
        `\n\nInternal id: ${code.id}`,
      result: null,
    }
  },
)
