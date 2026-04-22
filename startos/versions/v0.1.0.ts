// First version of the package. Migrations get added here as versions
// increment. For v0.1.0 there's nothing to migrate because nothing exists
// yet.

import { sdk } from '../sdk'

export const v0_1_0 = sdk.Version.of({
  version: '0.1.0:0',
  releaseNotes: `Initial release:\n` +
    `- Core licensing API: products, purchase, validate, revoke.\n` +
    `- BTCPay Server integration with HMAC-verified webhooks.\n` +
    `- Ed25519-signed license keys (offline-verifiable).\n` +
    `- Admin actions in StartOS UI.\n`,
  migrations: {
    up: async ({ effects }) => {},
    down: async ({ effects }) => {},
  },
})
