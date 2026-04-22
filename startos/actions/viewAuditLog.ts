// Action: view recent admin audit log entries.
//
// Every admin mutation writes an audit row recording: who (hashed bearer
// token), what (action slug), target id, client IP, user agent, and a
// free-form JSON detail blob. This action surfaces them in StartOS so the
// operator can skim without curl.

import { sdk } from '../sdk'
import { adminCall, LICENSING_URL } from '../utils'

const input = sdk.InputSpec.of({
  limit: {
    type: 'number',
    name: 'Limit',
    description: 'Number of most recent entries to return (1–1000).',
    required: true,
    default: 50,
    min: 1,
    max: 1000,
    integer: true,
  },
  action: {
    type: 'text',
    name: 'Filter action',
    description:
      'Optional action slug to filter on. E.g., "license.revoke", ' +
      '"license.suspend", "policy.create", "webhook_endpoint.create".',
    required: false,
    default: null,
  },
})

export const viewAuditLog = sdk.Action.withInput(
  'viewAuditLog',
  async ({ effects }) => ({
    name: 'View audit log',
    description:
      'Show the most recent admin mutations recorded by the service — ' +
      'useful for compliance, debugging, or checking what an API-key holder ' +
      'has been up to.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Diagnostics',
    visibility: 'enabled',
  }),
  input,
  async ({ effects, input: formInput }) => {
    const store = await sdk.store.getOwn(effects, sdk.StorePath).const()
    const params = new URLSearchParams()
    params.set('limit', String(formInput.limit))
    if (formInput.action) params.set('action', formInput.action)

    const resp = await adminCall(
      LICENSING_URL,
      store.admin_api_key,
      `/v1/admin/audit?${params.toString()}`,
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`Audit fetch failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      entries: Array<{
        id: string
        created_at: string
        action: string
        target_type: string | null
        target_id: string | null
        actor_hash: string | null
        client_ip: string | null
        detail: unknown
      }>
    }
    if (body.entries.length === 0) {
      return { message: 'No audit entries match the filter.' }
    }
    const lines = body.entries.map((e) => {
      const target = e.target_type && e.target_id ? `${e.target_type}:${e.target_id}` : '(no target)'
      const actor = e.actor_hash ? `actor=${e.actor_hash.slice(0, 10)}…` : 'actor=?'
      const ip = e.client_ip ? `ip=${e.client_ip}` : ''
      return `• ${e.created_at}  ${e.action}  ${target}  ${actor}  ${ip}`
    })
    return {
      message:
        `${body.entries.length} entry(ies):\n\n` + lines.join('\n'),
    }
  },
)
