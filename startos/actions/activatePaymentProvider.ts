// Switch the active payment provider WITHOUT re-running Connect.
// Use case: operator has both BTCPay and Zaprite configured (i.e.,
// they ran Connect on both at some point) and wants to flip which
// one currently handles purchases.
//
// One unified "Switch active payment provider" action with a
// dropdown — replaces the two earlier "Activate BTCPay" / "Activate
// Zaprite" actions, which were confusing because they appeared
// alongside Connect/Disconnect/Status and operators couldn't tell
// at a glance which one was currently active.
//
// If the chosen provider isn't yet configured, the daemon returns
// 400 with a "Run Connect first" message; we surface that to the
// operator unchanged.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const switchInput = InputSpec.of({
  provider: Value.select({
    name: 'Active provider',
    description:
      'Which connected payment provider should handle new purchases. ' +
      'The other provider stays configured (no need to re-run Connect ' +
      'if you switch back). Existing license keys are unaffected.',
    required: true,
    default: 'btcpay',
    values: {
      btcpay: 'BTCPay',
      zaprite: 'Zaprite',
    },
  }),
})

async function activate(provider: 'btcpay' | 'zaprite') {
  const storeData = await store.read().once()
  if (!storeData) throw new Error('Store not initialized — restart the service.')
  const resp = await adminCall(
    LICENSING_URL,
    storeData.admin_api_key,
    '/v1/admin/payment-provider/activate',
    {
      method: 'POST',
      body: JSON.stringify({ provider }),
    },
  )
  if (!resp.ok) {
    throw new Error(`Activate failed: HTTP ${resp.status} — ${await resp.text()}`)
  }
  const body = (await resp.json()) as { ok: true; active: string }
  return body
}

/** Unified switch — replaces the two single-purpose Activate actions. */
export const switchPaymentProvider = sdk.Action.withInput(
  'switch-payment-provider',
  async () => ({
    name: 'Switch active payment provider',
    description:
      'Flip which connected payment provider handles new purchases ' +
      '(BTCPay vs Zaprite). Use only when both are already configured. ' +
      "Existing license keys aren't affected by the swap.",
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Payment provider',
    visibility: 'enabled',
  }),
  switchInput,
  async ({ effects: _effects }) => {
    // Pre-fill from current active provider so the operator can
    // see what's set and only need to click if they want to change.
    const storeData = await store.read().once()
    if (!storeData) return { provider: 'btcpay' as const }
    try {
      const resp = await adminCall(
        LICENSING_URL,
        storeData.admin_api_key,
        '/v1/admin/payment-provider/status',
        { method: 'GET' },
      )
      if (resp.ok) {
        const body = (await resp.json()) as { active?: string }
        if (body.active === 'zaprite') return { provider: 'zaprite' as const }
      }
    } catch {
      // Status read failure shouldn't block the action.
    }
    return { provider: 'btcpay' as const }
  },
  async ({ effects: _effects, input }) => {
    const body = await activate(input.provider)
    const other = input.provider === 'btcpay' ? 'Zaprite' : 'BTCPay'
    const label = input.provider === 'btcpay' ? 'BTCPay' : 'Zaprite'
    return {
      version: '1',
      title: `${label} is now the active provider`,
      message:
        `Active payment provider is now ${body.active}. New purchases ` +
        `route through ${label}. ${other} remains configured but ` +
        `inactive until you switch again or disconnect it.`,
      result: null,
    }
  },
)

// Legacy single-purpose actions retained as deprecated shims so any
// operator scripts/links pointing at the old action ids still work
// after upgrade. The unified switchPaymentProvider above is the
// recommended path. Operators see only the new action in the StartOS
// UI (these aren't registered in actions/index.ts after this change).
export const activateBtcpay = sdk.Action.withoutInput(
  'activate-btcpay',
  async () => ({
    name: 'Activate BTCPay (legacy)',
    description:
      'Deprecated — use "Switch active payment provider" instead. ' +
      'Kept for backward compatibility with old scripts.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Payment provider',
    visibility: 'hidden',
  }),
  async () => {
    const body = await activate('btcpay')
    return {
      version: '1',
      title: 'BTCPay is now the active provider',
      message: `Active payment provider is now ${body.active}.`,
      result: null,
    }
  },
)

export const activateZaprite = sdk.Action.withoutInput(
  'activate-zaprite',
  async () => ({
    name: 'Activate Zaprite (legacy)',
    description:
      'Deprecated — use "Switch active payment provider" instead. ' +
      'Kept for backward compatibility with old scripts.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Payment provider',
    visibility: 'hidden',
  }),
  async () => {
    const body = await activate('zaprite')
    return {
      version: '1',
      title: 'Zaprite is now the active provider',
      message: `Active payment provider is now ${body.active}.`,
      result: null,
    }
  },
)
