// Action: search licenses by buyer email / Nostr npub / invoice id.
//
// The typical use case is "a buyer emailed me saying they lost their key."
// Operator runs this with the buyer's email and gets back up to 100
// matching licenses with IDs, product slugs, and current status.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { adminCall, LICENSING_URL } from '../utils'

const { InputSpec, Value } = sdk

const input = InputSpec.of({
  buyer_email: Value.text({
    name: 'Buyer email',
    description: 'Exact-match email address (leave blank if searching by another field).',
    required: false,
    default: null,
  }),
  nostr_npub: Value.text({
    name: 'Nostr npub',
    description: 'Nostr public key (npub…). Optional.',
    required: false,
    default: null,
  }),
  invoice_id: Value.text({
    name: 'BTCPay invoice ID',
    description: 'The BTCPay invoice ID associated with a purchase. Optional.',
    required: false,
    default: null,
  }),
})

export const searchLicenses = sdk.Action.withInput(
  'search-licenses',
  async () => ({
    name: 'Search licenses',
    description:
      "Look up a buyer's licenses by email, Nostr npub, or BTCPay " +
      'invoice ID. Intended for "lost key recovery" support requests.',
    warning: null,
    allowedStatuses: 'only-running',
    group: 'Licenses',
    visibility: 'enabled',
  }),
  input,
  async () => null,
  async ({ effects: _effects, input: formInput }) => {
    const storeData = await store.read().once()
    if (!storeData) throw new Error('Store not initialized — restart the service.')

    const params = new URLSearchParams()
    if (formInput.buyer_email) params.set('buyer_email', formInput.buyer_email)
    if (formInput.nostr_npub) params.set('nostr_npub', formInput.nostr_npub)
    if (formInput.invoice_id) params.set('invoice_id', formInput.invoice_id)
    if ([...params.keys()].length === 0) {
      throw new Error('Provide at least one search field (email, npub, or invoice).')
    }

    const resp = await adminCall(
      LICENSING_URL,
      storeData.admin_api_key,
      `/v1/admin/licenses/search?${params.toString()}`,
      { method: 'GET' },
    )
    if (!resp.ok) {
      throw new Error(`Search failed: HTTP ${resp.status} — ${await resp.text()}`)
    }
    const body = (await resp.json()) as {
      licenses: Array<{
        id: string
        product_id: string
        status: string
        buyer_email: string | null
        issued_at: string
        expires_at: string | null
      }>
    }
    if (body.licenses.length === 0) {
      return {
        version: '1',
        title: 'No matches',
        message: 'No licenses matched.',
        result: null,
      }
    }
    const lines = body.licenses.map(
      (l) =>
        `• ${l.id}  (${l.status})  product=${l.product_id}` +
        (l.buyer_email ? `  buyer=${l.buyer_email}` : '') +
        `  issued=${l.issued_at}` +
        (l.expires_at ? `  expires=${l.expires_at}` : ''),
    )
    return {
      version: '1',
      title: `Found ${body.licenses.length} license(s)`,
      message:
        `Found ${body.licenses.length} license(s):\n\n` +
        lines.join('\n') +
        '\n\nTo reissue the key to the buyer, look up the license details ' +
        'via /v1/admin/licenses with the admin API key.',
      result: null,
    }
  },
)
