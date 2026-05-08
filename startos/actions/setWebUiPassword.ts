// Action: set or rotate the web UI password.
//
// Until v0.1.0:28 the only way to sign into the admin web UI was to paste
// the admin API key into a localStorage-backed login form. This action
// lets the operator set a real password instead — argon2id-hashed and
// stored in the daemon's settings table. After it's set, the SPA login
// page shows a password field; existing API key continues to work for
// automation.
//
// Rotating the password invalidates all existing sessions (forced
// re-login). Minimum length: 12 characters, enforced server-side.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  password: Value.text({
    name: 'New password',
    description:
      'Minimum 12 characters. This is the password you will use to ' +
      'sign into the admin web UI at /admin/. Setting (or rotating) ' +
      'this invalidates any active web sessions — you will need to ' +
      'sign in again with the new password.',
    required: true,
    masked: true,
    minLength: 12,
    default: null,
    placeholder: '••••••••••••',
  }),
  confirm: Value.text({
    name: 'Confirm password',
    description: 'Re-type the password exactly to catch typos.',
    required: true,
    masked: true,
    default: null,
    placeholder: '••••••••••••',
  }),
})

export const setWebUiPassword = sdk.Action.withInput(
  'set-web-ui-password',
  async () => ({
    name: 'Set web UI password',
    description:
      'Set or change the password used to sign into the admin web UI. ' +
      'Replaces the API-key paste step on the login page.',
    warning:
      'Rotating the password signs out every active web session and ' +
      'forces a fresh login.',
    allowedStatuses: 'only-running',
    group: 'General',
    visibility: 'enabled',
  }),
  input,
  // No prefill — passwords are sensitive.
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')
    if (formInput.password !== formInput.confirm) {
      throw new Error('Passwords do not match. Re-type carefully.')
    }
    if (formInput.password.length < 12) {
      throw new Error('Password must be at least 12 characters.')
    }

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/web-password',
      { method: 'POST', body: JSON.stringify({ password: formInput.password }) },
    )
    if (!resp.ok) {
      throw new Error(
        `Password update failed: HTTP ${resp.status} — ${await resp.text()}`,
      )
    }

    return {
      version: '1',
      title: 'Web UI password set',
      message:
        'Password saved. Next time you visit the admin web UI, sign in ' +
        'with the new password. Any existing browser session was invalidated; ' +
        'all signed-in tabs need to log in again. The admin API key continues ' +
        'to work for automation.',
      result: null,
    }
  },
)
