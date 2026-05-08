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
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

export const configureBtcpay = sdk.Action.withoutInput(
  'configure-btcpay',
  async () => ({
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
  async () => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    // Idempotency guard: if Keysat is already connected to a BTCPay
    // store, re-running Connect would spawn a NEW webhook subscription
    // on BTCPay's side (because the authorize flow always registers
    // one). That leaves orphan webhooks pointing at this Keysat that
    // BTCPay will keep trying to deliver to, and confuses
    // reconciliation. Steer the operator to Disconnect first instead.
    try {
      const statusResp = await adminCall(
        LICENSING_URL,
        storeData.admin_api_key,
        '/v1/admin/btcpay/status',
        { method: 'GET' },
      )
      if (statusResp.ok) {
        const status = (await statusResp.json()) as {
          connected?: boolean
          store_id?: string | null
          base_url?: string | null
        }
        if (status.connected) {
          return {
            version: '1',
            title: 'BTCPay already connected',
            message:
              `Keysat is already connected to ` +
              `${status.base_url ?? '(unknown URL)'} ` +
              `(store ${status.store_id ?? '(unknown id)'}).\n\n` +
              `To re-authorize (e.g., switch stores or rotate the API key), ` +
              `run "Disconnect BTCPay" first, then re-run "Connect BTCPay". ` +
              `Existing license keys, products, and policies are unaffected ` +
              `by a Disconnect/Connect cycle.\n\n` +
              `If you're seeing connection problems, "Check BTCPay connection" ` +
              `also reports wallet / payment-method status that the connect ` +
              `flow doesn't surface.`,
            result: null,
          }
        }
      }
      // Status check failure is non-fatal — fall through to the
      // authorize flow. Same UX as before.
    } catch (_) {
      // Same — non-fatal.
    }

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/btcpay/connect',
      { method: 'POST' },
    )
    if (!resp.ok) {
      throw new Error(`Connect initialisation failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as { authorize_url: string }

    return {
      version: '1',
      title: 'Approve on BTCPay to finish connecting',
      message:
        'Open the URL below in your browser. You will be taken to your ' +
        'BTCPay Server, where you click "Authorize". After that BTCPay ' +
        'sends the API key back to Keysat automatically — you do not ' +
        'need to copy anything.\n\nYou can confirm the connection succeeded ' +
        'with the "Check BTCPay connection" action once approval is complete.',
      result: {
        type: 'single',
        value: body.authorize_url,
        copyable: true,
        qr: false,
        masked: false,
      },
    }
  },
)

/** Replay id used by init/index.ts when surfacing the BTCPay setup task. */
const BTCPAY_SETUP_TASK_ID = 'btcpay-initial-setup'

/** Disconnect BTCPay — clean revocation path for re-authorize cases. */
export const disconnectBtcpay = sdk.Action.withoutInput(
  'disconnect-btcpay',
  async () => ({
    name: 'Disconnect BTCPay',
    description:
      'Disconnect Keysat from your BTCPay Server: revoke the API key, ' +
      'delete the registered webhook, and clear local connection state. ' +
      "Run this before 'Connect BTCPay' if you want to re-authorize " +
      '(e.g., to switch stores or rotate the API key). Existing license ' +
      'keys, products, and policies are unaffected.',
    warning:
      'Until you re-run "Connect BTCPay" after this, new purchases will ' +
      'return 503 (BTCPay not configured). Already-issued license keys ' +
      'continue to validate normally.',
    allowedStatuses: 'only-running',
    group: 'BTCPay',
    visibility: 'enabled',
  }),
  async ({ effects: _effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/btcpay/disconnect',
      { method: 'POST' },
    )
    if (!resp.ok) {
      throw new Error(`Disconnect failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as
      | { ok: true; noop: true; message: string }
      | {
          ok: true
          noop: false
          store_id: string | null
          webhook_id: string | null
          warnings: string[]
        }
    if ('noop' in body && body.noop) {
      return {
        version: '1',
        title: 'Already disconnected',
        message: body.message,
        result: null,
      }
    }
    const b = body as {
      ok: true
      noop: false
      store_id: string | null
      webhook_id: string | null
      warnings: string[]
    }
    const warningsBlock = b.warnings.length > 0
      ? `\n\nWarnings:\n${b.warnings.map((w) => `• ${w}`).join('\n')}`
      : ''
    return {
      version: '1',
      title: 'BTCPay disconnected',
      message:
        `Local BTCPay connection cleared. ` +
        `Store id was ${b.store_id ?? '(unknown)'}, webhook id was ${b.webhook_id ?? '(none)'}. ` +
        `You can now run "Connect BTCPay" again to re-authorize.${warningsBlock}`,
      result: null,
    }
  },
)

/** Optional companion action: show current BTCPay connection state. */
export const btcpayStatus = sdk.Action.withoutInput(
  'btcpay-status',
  async () => ({
    name: 'Check BTCPay connection',
    description: 'Shows whether BTCPay is currently connected, and the store id.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'BTCPay',
    visibility: 'enabled',
  }),
  async ({ effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
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
        version: '1',
        title: 'Not connected',
        message: 'BTCPay is not connected yet. Run the "Connect BTCPay" action to authorize.',
        result: null,
      }
    }

    // BTCPay is connected — clear the install-time setup task so it
    // disappears from the dashboard. clearTask is idempotent and
    // tolerates being called when no such task exists, so this is safe
    // every time btcpayStatus is run.
    try {
      await sdk.action.clearTask(effects, BTCPAY_SETUP_TASK_ID)
    } catch (_) {
      // Non-fatal — we still report status.
    }

    // Also check whether BTCPay's store has any payment methods (wallet
    // / Lightning) configured. A connected store with zero payment
    // methods can't actually issue invoices — that's the trap that
    // surfaces as "BTC-CHAIN: Payment method unavailable" when buyers
    // try to purchase. Surface the situation here so the operator
    // discovers it BEFORE a customer hits a broken purchase flow.
    let walletNote = ''
    try {
      const pmResp = await adminCall(
        LICENSING_URL,
        storeData.admin_api_key,
        '/v1/admin/btcpay/payment-methods',
        { method: 'GET' },
      )
      if (pmResp.ok) {
        const pmBody = (await pmResp.json()) as { count: number }
        if (pmBody.count === 0) {
          walletNote =
            `\n\n⚠ NO WALLET CONFIGURED on this BTCPay store. Buyers won't ` +
            `be able to pay until you set one up.\n` +
            `Open your BTCPay store settings (${body.base_url.replace(/^http:\/\//, 'http://').replace(/^https:\/\//, 'https://')}/stores/${body.store_id}) ` +
            `→ Wallets / Lightning, then come back and re-run "Check BTCPay connection".`
        } else {
          walletNote = `\n\n✓ ${pmBody.count} payment method(s) configured.`
        }
      }
    } catch (_) {
      // Non-fatal: payment-method check is informational.
    }

    return {
      version: '1',
      title: 'BTCPay is connected',
      message:
        `Store id: ${body.store_id}\n` +
        `Webhook id: ${body.webhook_id ?? '(not registered — check BTCPay manually)'}\n` +
        `Base URL: ${body.base_url}` +
        walletNote,
      result: null,
    }
  },
)
