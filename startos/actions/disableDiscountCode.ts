// Action: disable (or re-enable) a discount code.
//
// Disabling is reversible — the code's redemption history is preserved
// either way. Disabled codes simply won't redeem on new purchases.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  code: Value.text({
    name: 'Code',
    description:
      'The redeemable string (e.g. "FOUNDERS50"). Case-insensitive — ' +
      'will be uppercased before lookup.',
    required: true,
    default: null,
  }),
  active: Value.toggle({
    name: 'Active',
    description:
      'Toggle off to disable this code. Toggle on to re-enable a ' +
      'previously disabled code.',
    default: false,
  }),
})

export const disableDiscountCode = sdk.Action.withInput(
  'disable-discount-code',
  async () => ({
    name: 'Disable / enable discount code',
    description:
      'Disable a code so it stops accepting new redemptions. Existing ' +
      'redemptions and the underlying license are unaffected. Re-enable ' +
      'by running again with "Active" toggled on.',
    warning:
      'Disabling does not refund or revoke previously-issued licenses. ' +
      'Use "Revoke license" for that.',
    allowedStatuses: 'only-running',
    group: 'Discount codes',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    // Look the code up by string to discover its id.
    const lookup = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      '/v1/admin/discount-codes',
      { method: 'GET' },
    )
    if (!lookup.ok) {
      throw new Error(`Lookup failed: HTTP ${lookup.status} — ${await lookup.text()}`)
    }
    const body = (await lookup.json()) as { codes: Array<{ id: string; code: string }> }
    const target = body.codes.find(
      (c) => c.code.toUpperCase() === formInput.code.trim().toUpperCase(),
    )
    if (!target) {
      throw new Error(
        `No discount code found matching "${formInput.code}". ` +
          'Use "List discount codes" with "Include disabled codes" toggled on to see all codes.',
      )
    }

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      `/v1/admin/discount-codes/${target.id}/active`,
      { method: 'PATCH', body: JSON.stringify({ active: formInput.active }) },
    )
    if (!resp.ok) {
      throw new Error(`Update failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    return {
      version: '1',
      title: formInput.active ? 'Code re-enabled' : 'Code disabled',
      message: formInput.active
        ? `Code "${target.code}" is now active and will accept new redemptions.`
        : `Code "${target.code}" is now disabled. New purchases that try ` +
          'to redeem it will be rejected. Existing redemptions and licenses are unaffected.',
      result: null,
    }
  },
)
