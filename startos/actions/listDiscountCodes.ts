// Action: list discount / referral codes with usage stats.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  include_inactive: Value.toggle({
    name: 'Include disabled codes',
    description: 'Show codes that have been disabled.',
    default: false,
  }),
})

interface DiscountCode {
  id: string
  code: string
  kind: string
  amount: number
  max_uses: number | null
  used_count: number
  expires_at: string | null
  applies_to_product_id: string | null
  applies_to_policy_id: string | null
  referrer_label: string | null
  description: string
  active: boolean
  created_at: string
}

export const listDiscountCodes = sdk.Action.withInput(
  'list-discount-codes',
  async () => ({
    name: 'List discount codes',
    description: 'View every discount / referral code with usage stats.',
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

    const params = new URLSearchParams()
    if (formInput.include_inactive) params.set('include_inactive', 'true')
    const path =
      '/v1/admin/discount-codes' +
      (params.toString() ? `?${params.toString()}` : '')

    const resp = await adminCall(LICENSING_URL, storeData.admin_api_key, path, {
      method: 'GET',
    })
    if (!resp.ok) {
      throw new Error(`List failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as { codes: DiscountCode[] }
    if (body.codes.length === 0) {
      return {
        version: '1',
        title: 'No discount codes',
        message:
          formInput.include_inactive
            ? 'No discount codes exist yet.'
            : 'No active discount codes. Toggle "Include disabled codes" to also see disabled ones.',
        result: null,
      }
    }
    const lines = body.codes.map((c) => {
      const off =
        c.kind === 'percent'
          ? `${(c.amount / 100).toFixed(c.amount % 100 === 0 ? 0 : 2)}% off`
          : c.kind === 'fixed_sats'
            ? `${c.amount} sats off`
            : 'free license'
      const usage = c.max_uses
        ? `${c.used_count}/${c.max_uses}`
        : `${c.used_count}/∞`
      const status = c.active ? 'active' : 'DISABLED'
      const exp = c.expires_at ? `expires ${c.expires_at}` : 'no expiry'
      const target = c.applies_to_product_id
        ? c.applies_to_policy_id
          ? '(product+policy scoped)'
          : '(product scoped)'
        : '(any product)'
      const ref = c.referrer_label ? ` — ref:${c.referrer_label}` : ''
      return `• ${c.code}  [${status}]  ${off}  uses ${usage}  ${exp}  ${target}${ref}`
    })
    return {
      version: '1',
      title: `${body.codes.length} discount code(s)`,
      message: lines.join('\n'),
      result: null,
    }
  },
)
