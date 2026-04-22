// Daemon definition — the thing that actually runs when the service is
// started. Passes configuration into the Rust binary via environment
// variables, same interface as `.env.example` in the upstream project.

import { sdk } from './sdk'

export const main = sdk.setupMain(async ({ effects, started }) => {
  const store = await sdk.store.getOwn(effects, sdk.StorePath).const()

  // Public URL the service advertises to buyers / referenced in webhooks.
  // We read our own primary interface address from StartOS at runtime so
  // this works whether the operator exposes on Tor, LAN, or clearnet.
  const publicUrl = await sdk.serviceInterface
    .getOwn(effects, 'api')
    .const()
    .then((i) => i?.addressInfo?.urls?.[0] ?? 'http://localhost:8080')

  const sub = await sdk.SubContainer.of(
    effects,
    { imageId: 'main' },
    [{ mountpoint: '/data', volumeId: 'main', subpath: null, readonly: false }],
    'keysat',
  )

  return sdk.Daemons.of({ effects, started, healthReceipts: [] }).addDaemon('primary', {
    subcontainer: sub,
    exec: {
      command: sdk.useEntrypoint(),
      env: {
        KEYSAT_BIND: '0.0.0.0:8080',
        KEYSAT_DB_PATH: '/data/keysat.db',
        KEYSAT_PUBLIC_URL: publicUrl,
        KEYSAT_ADMIN_API_KEY: store.admin_api_key,
        KEYSAT_OPERATOR_NAME: store.operator_name,
        // Reachable because of our dependency on btcpayserver.
        BTCPAY_URL: 'http://btcpayserver.startos:23000',
        // The three credentials below are left empty in the normal case —
        // the daemon now persists them in its own DB after the one-click
        // "Connect BTCPay" action completes. Only seed them here if you are
        // migrating from a pre-authorize-flow install.
        BTCPAY_API_KEY: '',
        BTCPAY_STORE_ID: '',
        BTCPAY_WEBHOOK_SECRET: '',
        RUST_LOG: 'info,sqlx=warn,hyper=warn',
      },
    },
    ready: {
      display: 'API',
      fn: () =>
        sdk.healthCheck.checkPortListening(effects, 8080, {
          successMessage: 'Keysat API is accepting requests',
          errorMessage: 'Keysat API is not responding on port 8080',
        }),
    },
    requires: [],
  })
})
