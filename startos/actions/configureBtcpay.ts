// Action: one-click "Connect BTCPay".
//
// Instead of asking the operator to generate and paste an API key, we use
// BTCPay's built-in authorize flow:
//   1. Action calls POST /v1/admin/btcpay/connect on the local daemon.
//   2. Daemon returns an authorize URL pointing at the sibling BTCPay
//      instance, with the permissions we need pre-filled.
//   3. Operator opens that URL in their browser, approves on BTCPay's
//      consent page, and BTCPay calls back into /v1/btcpay/authorize/callback
//      with the freshly-minted API key.
//   4. Daemon auto-detects the store, registers the webhook, and persists
//      everything.
//
// The operator never sees or types an API key, store id, or webhook secret.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

export const configureBtcpay = sdk.Action.withoutInput(
  'configureBtcpay',
  async ({ effects }) => ({
    name: 'Connect BTCPay',
    description:
      "One-click connect to your BTCPay Server. Opens a consent page in " +
      "your browser where you click 'Authorize'; Keysat then auto-detects " +
      "your store and registers the webhook.",
    warning: null,
    allowedStatuses: 'only-running',
    group: 'BTCPay',
    visibility: 'enabled',
  }),
  async ({ effects }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      '/v1/admin/btcpay/connect',
      { method: 'POST' },
    )
    if (!resp.ok) {
      throw new Error(`Connect initialisation failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as { authorize_url: string }

    return {
      message:
        'Open the URL below in your browser. You will be taken to your ' +
        'BTCPay Server, where you click "Authorize". After that BTCPay ' +
        'sends the API key back to Keysat automatically — you do not ' +
        'need to copy anything.\n\n' +
        body.authorize_url +
        '\n\nYou can confirm the connection succeeded with the "Check BTCPay ' +
        'connection" action once approval is complete.',
    }
  },
)

/** Optional companion action: show current BTCPay connection state. */
export const btcpayStatus = sdk.Action.withoutInput(
  'btcpayStatus',
  async ({ effects }) => ({
    name: 'Check BTCPay connection',
    description: 'Shows whether BTCPay is currently connected, and the store id.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'BTCPay',
    visibility: 'enabled',
  }),
  async ({ effects }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      '/v1/admin/btcpay/status',
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`Status check failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as
      | { connected: false }
      | { connected: true; store_id: string; webhook_id: string | null; base_url: string }

    if (!body.connected) {
      return {
        message: 'BTCPay is not connected yet. Run the "Connect BTCPay" action to authorize.',
      }
    }
    return {
      message:
        `BTCPay is connected.\n` +
        `Store id: ${body.store_id}\n` +
        `Webhook id: ${body.webhook_id ?? '(not registered — check BTCPay manually)'}\n` +
        `Base URL: ${body.base_url}`,
    }
  },
)
