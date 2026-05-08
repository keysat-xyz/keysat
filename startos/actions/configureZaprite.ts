// Action: connect / disconnect / status for Zaprite as the active
// payment provider.
//
// Unlike BTCPay's authorize flow (OAuth-style consent redirect),
// Zaprite doesn't expose a programmatic authorize endpoint. The
// operator creates an API key in their Zaprite dashboard at
// app.zaprite.com/.../settings/api, then pastes it into the form
// here. The daemon validates the key by pinging Zaprite's API,
// then persists + swaps the active provider atomically.
//
// Webhook setup is operator-side: after connecting, the operator
// adds a webhook in Zaprite's dashboard pointing at
// <their-keysat-public-url>/v1/zaprite/webhook. There's no
// signing secret — see the daemon's payment::zaprite module
// comment for the security model (externalUniqId round-trip).

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const connectInput = InputSpec.of({
  api_key: Value.text({
    name: 'Zaprite API key',
    description:
      'Create an API key at app.zaprite.com → Settings → API. ' +
      'One key per Keysat instance. The key is stored in your ' +
      'StartOS volume (encrypted at rest by StartOS) and never ' +
      'transmitted off your server.',
    required: true,
    default: null,
    masked: true,
  }),
  base_url: Value.text({
    name: 'API base URL',
    description:
      'Defaults to https://api.zaprite.com — only override for ' +
      'sandbox organizations or future regional endpoints.',
    required: false,
    default: 'https://api.zaprite.com',
  }),
})

export const configureZaprite = sdk.Action.withInput(
  'configure-zaprite',
  async () => ({
    name: 'Connect Zaprite',
    description:
      'Connect Keysat to your Zaprite account so buyers can pay with ' +
      'cards (USD/EUR) and Bitcoin via your Zaprite-connected wallets. ' +
      'Use INSTEAD OF Connect BTCPay — only one payment provider can ' +
      'be active at a time. Disconnect first if switching providers.',
    warning:
      'Switching providers does not affect already-issued license keys; ' +
      'they continue to validate normally. New purchases route through ' +
      'whichever provider is active at the time of checkout.',
    allowedStatuses: 'only-running',
    group: 'Zaprite',
    visibility: 'enabled',
  }),
  connectInput,
  // Pre-fill base_url; never pre-fill api_key (force operator to paste fresh).
  async ({ effects: _effects }) => ({
    api_key: '',
    base_url: 'https://api.zaprite.com',
  }),
  async ({ effects: _effects, input }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/zaprite/connect',
      {
        method: 'POST',
        body: JSON.stringify({
          api_key: input.api_key.trim(),
          base_url: (input.base_url || 'https://api.zaprite.com').trim(),
        }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Connect failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      ok: true
      provider: string
      base_url: string
    }
    return {
      version: '1',
      title: 'Zaprite connected',
      message:
        `Active payment provider is now Zaprite (${body.base_url}).\n\n` +
        `Next step: register a webhook in Zaprite's dashboard pointing at:\n` +
        `<your Keysat public URL>/v1/zaprite/webhook\n\n` +
        `Zaprite doesn't sign webhook deliveries; Keysat authenticates ` +
        `each delivery via the externalUniqId we attach at order ` +
        `creation, so a webhook configured to ANY URL on your daemon ` +
        `is safe even without a shared secret.`,
      result: null,
    }
  },
)

/** Counterpart to Connect — clears stored credentials + active provider. */
export const disconnectZaprite = sdk.Action.withoutInput(
  'disconnect-zaprite',
  async () => ({
    name: 'Disconnect Zaprite',
    description:
      'Disconnect Keysat from your Zaprite account. Wipes the stored ' +
      'API key and clears the active provider. Existing license keys ' +
      'are unaffected. Run this before re-running Connect Zaprite if ' +
      'you want to rotate the key or switch organizations.',
    warning:
      "Don't forget to also delete the corresponding webhook in your " +
      "Zaprite dashboard — Keysat can't programmatically delete it for " +
      'you because the webhook-management API surface is not on the ' +
      'public Zaprite OpenAPI we have access to.',
    allowedStatuses: 'only-running',
    group: 'Zaprite',
    visibility: 'enabled',
  }),
  async ({ effects: _effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/zaprite/disconnect',
      { method: 'POST' },
    )
    if (!resp.ok) {
      throw new Error(`Disconnect failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as
      | { ok: true; noop: true; message: string }
      | { ok: true; noop: false; message: string }
    return {
      version: '1',
      title: body.noop ? 'Already disconnected' : 'Zaprite disconnected',
      message: body.message,
      result: null,
    }
  },
)

/** Quick read-only check of the current connection state. */
export const zapriteStatus = sdk.Action.withoutInput(
  'zaprite-status',
  async () => ({
    name: 'Check Zaprite connection',
    description: 'Show whether Zaprite is the active payment provider and the configured base URL.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Zaprite',
    visibility: 'enabled',
  }),
  async ({ effects: _effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/zaprite/status',
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`Status check failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      connected: boolean
      active_provider: string | null
      base_url: string | null
      webhook_id: string | null
    }
    if (!body.connected) {
      return {
        version: '1',
        title: 'Zaprite not connected',
        message: 'No Zaprite credentials configured. Run "Connect Zaprite" to paste in an API key.',
        result: null,
      }
    }
    return {
      version: '1',
      title: body.active_provider === 'zaprite' ? 'Zaprite is active' : 'Zaprite configured (provider not active)',
      message:
        `Connected to ${body.base_url ?? '(unknown URL)'}.\n` +
        `Active provider: ${body.active_provider ?? '(none)'}.` +
        (body.active_provider === 'zaprite'
          ? ''
          : "\n\nA different provider (likely BTCPay) is currently active. Disconnect that one first if you want Zaprite to take over."),
      result: null,
    }
  },
)
