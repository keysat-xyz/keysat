// Action: set the operator display name shown on the service homepage.
//
// As of v0.1.0:7+ this writes to the daemon's runtime settings table via
// the admin API, so changes take effect immediately without a daemon
// restart. We also mirror the value to the wrapper's package store so
// the StartOS prefill / future env-var handoff remains consistent.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  operator_name: Value.text({
    name: 'Operator name',
    description:
      'Displayed on the service homepage so buyers know whose Keysat ' +
      'instance they are interacting with. E.g., your name or business name.',
    required: true,
    default: null,
  }),
})

export const setOperatorName = sdk.Action.withInput(
  'set-operator-name',
  async () => ({
    name: 'Set operator name',
    description: 'Edit the operator name shown publicly. Takes effect immediately — no restart required.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'General',
    visibility: 'enabled',
  }),
  input,
  // Pre-fill the form with the current value.
  async ({ effects: _effects }) => {
    const current = await store.read().once()
    return current?.operator_name ? { operator_name: current.operator_name } : null
  },
  async ({ effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    const trimmed = formInput.operator_name.trim()

    // Live-update the daemon via admin endpoint. This stores the value
    // in the daemon's settings table and the very next request to / or
    // /thank-you uses it. No restart needed.
    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/settings/operator-name',
      { method: 'POST', body: JSON.stringify({ name: trimmed }) },
    )
    if (!resp.ok) {
      throw new Error(
        `Operator-name update failed: HTTP ${resp.status} — ${await resp.text()}`,
      )
    }

    // Mirror to the wrapper store. This isn't strictly required (the
    // daemon owns the live value), but it keeps the prefill working
    // and gives us a fallback path during package upgrades.
    await store.merge(effects, { operator_name: trimmed })

    return {
      version: '1',
      title: 'Operator name updated',
      message:
        `Operator name set to "${trimmed}". The change is live immediately — ` +
        `no restart needed. Anyone visiting your service homepage from now on will see the new name.`,
      result: null,
    }
  },
)
