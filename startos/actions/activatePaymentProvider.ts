// Switch the active payment provider WITHOUT re-running Connect.
// Use case: operator has both BTCPay and Zaprite configured (i.e.,
// they ran Connect on both at some point) and wants to flip which
// one currently handles purchases. Two convenience actions —
// "Activate BTCPay" / "Activate Zaprite" — each POSTs to the
// daemon's /v1/admin/payment-provider/activate endpoint.
//
// If the named provider isn't yet configured, the daemon returns
// 400 with a "Run Connect first" message; we surface that to the
// operator unchanged.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

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

export const activateBtcpay = sdk.Action.withoutInput(
  'activate-btcpay',
  async () => ({
    name: 'Activate BTCPay',
    description:
      'Switch the active payment provider to BTCPay. Use this if both ' +
      'BTCPay and Zaprite are already connected and you want to flip ' +
      "which one handles new purchases. Existing license keys aren't " +
      'affected by the swap.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'BTCPay',
    visibility: 'enabled',
  }),
  async () => {
    const body = await activate('btcpay')
    return {
      version: '1',
      title: 'BTCPay is now the active provider',
      message:
        `Active payment provider is now ${body.active}. New purchases ` +
        `route through BTCPay. Zaprite remains configured but inactive ` +
        `until you run "Activate Zaprite" or "Disconnect Zaprite".`,
      result: null,
    }
  },
)

export const activateZaprite = sdk.Action.withoutInput(
  'activate-zaprite',
  async () => ({
    name: 'Activate Zaprite',
    description:
      'Switch the active payment provider to Zaprite. Use this if both ' +
      'BTCPay and Zaprite are already connected and you want to flip ' +
      "which one handles new purchases. Existing license keys aren't " +
      'affected by the swap.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Zaprite',
    visibility: 'enabled',
  }),
  async () => {
    const body = await activate('zaprite')
    return {
      version: '1',
      title: 'Zaprite is now the active provider',
      message:
        `Active payment provider is now ${body.active}. New purchases ` +
        `route through Zaprite. BTCPay remains configured but inactive ` +
        `until you run "Activate BTCPay" or "Disconnect BTCPay".`,
      result: null,
    }
  },
)
