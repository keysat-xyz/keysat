// Action: list currently registered outbound webhook endpoints.
//
// Shows each endpoint's id, URL, event list, and active flag. Secrets are
// masked — rotate by deleting and recreating an endpoint.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

export const listWebhooks = sdk.Action.withoutInput(
  'list-webhooks',
  async () => ({
    name: 'List webhook endpoints',
    description: 'Show all currently-registered outbound webhook subscribers.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Webhooks',
    visibility: 'enabled',
  }),
  async ({ effects: _effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/webhook-endpoints',
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`List webhooks failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      endpoints: Array<{
        id: string
        url: string
        event_types: string[]
        active: number | boolean
        description: string
      }>
    }
    if (body.endpoints.length === 0) {
      return {
        version: '1',
        title: 'No webhooks',
        message:
          'No webhook endpoints registered. Use "Register webhook endpoint" ' +
          'to add one.',
        result: null,
      }
    }
    const lines = body.endpoints.map((ep) => {
      const activeStr = ep.active === true || ep.active === 1 ? 'active' : 'disabled'
      return `• ${ep.id}  [${activeStr}]  ${ep.url}  events=${ep.event_types.join(',')}` +
        (ep.description ? `  ("${ep.description}")` : '')
    })
    return {
      version: '1',
      title: `${body.endpoints.length} endpoint(s)`,
      message:
        `${body.endpoints.length} endpoint(s):\n\n` + lines.join('\n'),
      result: null,
    }
  },
)
