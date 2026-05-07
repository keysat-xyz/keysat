// Action: list machines (installs) currently bound to a license.
//
// Useful when a buyer asks "which devices am I active on?" or when
// troubleshooting a multi-seat cap ("can't activate, too many machines").

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  license_id: Value.text({
    name: 'License ID',
    description: 'UUID of the license to inspect.',
    required: true,
    default: null,
  }),
  include_inactive: Value.toggle({
    name: 'Include deactivated machines',
    description: 'Show rows for machines that were previously deactivated.',
    default: false,
  }),
})

export const listMachines = sdk.Action.withInput(
  'list-machines',
  async () => ({
    name: 'List machines',
    description: 'Show installs currently bound to a license.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Machines',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const params = new URLSearchParams()
    params.set('license_id', formInput.license_id)
    if (formInput.include_inactive) params.set('include_inactive', 'true')

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      `/v1/admin/machines?${params.toString()}`,
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`List machines failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      machines: Array<{
        id: string
        active: number | boolean
        hostname: string | null
        platform: string | null
        last_heartbeat_at: string | null
        activated_at: string
        fingerprint_hash: string
      }>
    }
    if (body.machines.length === 0) {
      return {
        version: '1',
        title: 'No machines',
        message: 'No machines bound to this license.',
        result: null,
      }
    }
    const lines = body.machines.map((m) => {
      const activeStr =
        m.active === true || m.active === 1 ? 'ACTIVE' : 'deactivated'
      const bits = [
        m.id,
        activeStr,
        m.hostname ?? 'unknown host',
        m.platform ?? '',
        `fp=${m.fingerprint_hash.slice(0, 12)}…`,
        `last_hb=${m.last_heartbeat_at ?? 'never'}`,
      ]
      return '• ' + bits.filter(Boolean).join('  ')
    })
    return {
      version: '1',
      title: `${body.machines.length} machine(s)`,
      message:
        `${body.machines.length} machine(s) on license ${formInput.license_id}:\n\n` +
        lines.join('\n') +
        '\n\nTo free a seat, use the "Deactivate machine" action with the machine id.',
      result: null,
    }
  },
)
