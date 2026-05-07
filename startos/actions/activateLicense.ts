// Action: activate the Keysat package's own self-license.
//
// The daemon embeds the keysat.xyz master public key at compile time
// (see licensing-service/src/license_self.rs). The operator pastes a
// LIC1-… key here; the daemon verifies it against the master pubkey,
// writes it to /data/keysat-license.txt, and swaps its runtime tier
// to Licensed without a restart.
//
// In permissive builds (the default for local `make x86`) the daemon
// will start regardless and this action just records the tier. In
// enforce builds (compiled with KEYSAT_LICENSE_ENFORCE=1, used for
// the marketplace .s9pk) the daemon refuses to start without a valid
// license, and this action is the bootstrap path: install Keysat,
// run this action with your activation key, then start the service.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  license_key: Value.text({
    name: 'License key',
    description:
      'Paste the LIC1-… license key issued for your Keysat install. ' +
      'Buy or redeem one at registry.keysat.xyz.',
    required: true,
    default: null,
    placeholder: 'LIC1-XXXXXXXXXXXX-XXXXXXXXXXXX',
  }),
})

export const activateLicense = sdk.Action.withInput(
  'activate-license',
  async () => ({
    name: 'Activate Keysat license',
    description:
      'Activate this Keysat install. Required for marketplace builds; ' +
      'optional but recommended for source-built dev installs (signals support, ' +
      'and lets the admin UI show your tier).',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'License',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    const key = formInput.license_key.trim()
    if (!key) {
      throw new Error('License key is required.')
    }

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/self-license',
      { method: 'POST', body: JSON.stringify({ license_key: key }) },
    )
    if (!resp.ok) {
      const body = await resp.text()
      let detail = body
      try {
        const parsed = JSON.parse(body)
        if (parsed.detail) detail = parsed.detail
        else if (parsed.error) detail = parsed.error
      } catch (_) {}
      throw new Error(`Activation rejected (HTTP ${resp.status}): ${detail}`)
    }

    const result = (await resp.json()) as {
      ok: boolean
      tier: {
        tier: 'licensed' | 'unlicensed'
        license_id?: string
        product_id?: string
        expires_at?: number
        entitlements?: string[]
        mode: string
      }
      message: string
    }

    return {
      version: '1',
      title: 'Keysat license activated',
      message:
        result.message +
        ' The license is stored at /data/keysat-license.txt and survives upgrades and reinstalls (it is part of your StartOS backup set).',
      result: null,
    }
  },
)

// Companion read-only action: surface the current self-license tier.
// Useful both as a sanity check after activation and as a way for the
// operator to see "am I running licensed or unlicensed?" without
// digging into logs.
export const showLicenseStatus = sdk.Action.withoutInput(
  'show-license-status',
  async () => ({
    name: 'Show Keysat license status',
    description:
      'Reports whether this Keysat install is running licensed or unlicensed, ' +
      'and which entitlements are active.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'License',
    visibility: 'enabled',
  }),
  async ({ effects: _effects }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/self-license',
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`Could not read license status: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const j = (await resp.json()) as {
      tier: 'licensed' | 'unlicensed'
      license_id?: string
      product_id?: string
      expires_at?: number
      entitlements?: string[]
      reason?: string
      mode: string
    }

    if (j.tier === 'licensed') {
      const exp = !j.expires_at
        ? 'perpetual'
        : new Date(j.expires_at * 1000).toISOString().slice(0, 10)
      const ents = (j.entitlements || []).length === 0 ? '(none)' : (j.entitlements || []).join(', ')
      return {
        version: '1',
        title: 'Licensed',
        message:
          `License id: ${j.license_id}\n` +
          `Expires: ${exp}\n` +
          `Entitlements: ${ents}\n` +
          `Build mode: ${j.mode}`,
        result: null,
      }
    } else {
      return {
        version: '1',
        title: 'Unlicensed',
        message:
          `Reason: ${j.reason || 'no license configured'}\n` +
          `Build mode: ${j.mode}\n\n` +
          (j.mode === 'enforce'
            ? 'This is a marketplace build that requires a valid license to run. Use the "Activate Keysat license" action to bootstrap.'
            : 'This is a permissive (dev) build. The daemon will keep running. Activate a license to see your tier reflected here.'),
        result: null,
      }
    }
  },
)
