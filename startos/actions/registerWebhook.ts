// Action: register an outbound webhook subscriber.
//
// After registration, Keysat will POST signed JSON bodies to the URL when
// relevant events fire (license.issued, license.revoked, machine.activated,
// machine.deactivated, invoice.settled, etc.). Signatures use HMAC-SHA256
// over the body, carried in the `X-Keysat-Signature` header as
// `sha256=<hex>` — same shape as BTCPay's outbound hooks.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  url: {
    type: 'text',
    name: 'Webhook URL',
    description: 'HTTPS endpoint that will receive POSTed event bodies.',
    required: true,
    default: null,
    patterns: [{ regex: '^https?://', description: 'must be an HTTP(S) URL' }],
  },
  event_types: {
    type: 'text',
    name: 'Event types',
    description:
      'Comma-separated list of events to subscribe to, or "*" for all. ' +
      'E.g., "license.issued,license.revoked". Known events: license.issued, ' +
      'license.revoked, license.suspended, license.unsuspended, ' +
      'machine.activated, machine.deactivated, invoice.settled.',
    required: true,
    default: '*',
  },
  description: {
    type: 'text',
    name: 'Description',
    description: 'Free-form label, shown in the admin list.',
    required: false,
    default: null,
  },
})

export const registerWebhook = sdk.Action.withInput(
  'registerWebhook',
  async ({ effects }) => ({
    name: 'Register webhook endpoint',
    description:
      'Tell Keysat to POST signed event notifications to an HTTPS URL you ' +
      'control. A fresh HMAC secret is generated and shown once — save it.',
    warning:
      'The HMAC secret is returned in plaintext exactly once, on creation. ' +
      'If you lose it, you will need to delete and recreate the endpoint.',
    allowedStatuses: 'only-running',
    group: 'Webhooks',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const eventTypes = formInput.event_types
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
    if (eventTypes.length === 0) {
      throw new Error('Provide at least one event type (or "*" for all).')
    }
    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      '/v1/admin/webhook-endpoints',
      {
        method: 'POST',
        body: JSON.stringify({
          url: formInput.url,
          event_types: eventTypes,
          description: formInput.description ?? '',
        }),
      },
    )
    if (!resp.ok) {
      throw new Error(`Register webhook failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const ep = (await resp.json()) as {
      id: string
      url: string
      secret: string
      event_types: string[]
    }
    return {
      message:
        `Registered webhook endpoint (id ${ep.id}).\n` +
        `URL: ${ep.url}\n` +
        `Events: ${ep.event_types.join(', ')}\n\n` +
        `HMAC secret (save this now — will not be shown again):\n${ep.secret}\n\n` +
        `Verify incoming requests with header X-Keysat-Signature: sha256=<hex> ` +
        `(HMAC-SHA256 of the raw request body using this secret).`,
    }
  },
)
