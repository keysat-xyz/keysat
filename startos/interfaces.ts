// Network interfaces exposed by the service.
//
// Three logical interfaces, all sharing the same internal port (8080).
// The Rust daemon routes by path, and StartOS uses the interface
// concept for *access surfaces* and *display grouping*.
//
//   - `api`        — the REST API that buyers (purchase flow) and licensed
//                    software (validate flow) hit. Must be reachable from
//                    outside the host if you're selling to the public.
//   - `webhook`    — the BTCPay webhook landing endpoint. Only BTCPay needs
//                    to reach it; same-host LAN is sufficient.
//   - `admin-ui`   — the embedded admin web UI (rust-embed at /admin/).
//                    type: 'ui' so StartOS surfaces a "Launch UI" button.
//                    Operator should restrict this interface's exposure
//                    to LAN-only or Tor-only — the public clearnet
//                    doesn't need to see it. (For v0.2 follow-up: split
//                    onto a separate port so it can be fully isolated
//                    from the public api.)

import { sdk } from './sdk'

export const setInterfaces = sdk.setupInterfaces(async ({ effects }) => {
  const multi = sdk.MultiHost.of(effects, 'api-multi')
  const origin = await multi.bindPort(8080, {
    protocol: 'http',
    preferredExternalPort: 443,
  })

  const api = sdk.createInterface(effects, {
    name: 'Licensing API',
    id: 'api',
    description:
      'REST API for buyers and licensed software. Public-facing: this is ' +
      'the URL you share with customers and bake into your own software ' +
      'builds as the licensing endpoint.',
    type: 'api',
    masked: false,
    schemeOverride: null,
    username: null,
    path: '',
    query: {},
  })

  const webhook = sdk.createInterface(effects, {
    name: 'BTCPay webhook endpoint',
    id: 'webhook',
    description:
      'The landing URL for BTCPay webhook callbacks. Not intended for ' +
      'human use — Keysat registers this URL with BTCPay automatically ' +
      'during the one-click "Connect BTCPay" flow.',
    type: 'api',
    masked: false,
    schemeOverride: null,
    username: null,
    path: '/btcpay',
    query: {},
  })

  const adminUi = sdk.createInterface(effects, {
    name: 'Admin Web UI',
    id: 'admin-ui',
    description:
      'Embedded admin dashboard — manage products, policies, discount ' +
      'codes, licenses, machines, webhooks, and audit log without ' +
      'leaving the browser. Login is gated by your Keysat admin API key. ' +
      'Recommended: restrict this interface to LAN or Tor only; the ' +
      'public clearnet does not need to reach the admin UI.',
    type: 'ui',
    masked: false,
    schemeOverride: null,
    username: null,
    path: '/admin',
    query: {},
  })

  const receipt = await origin.export([api, webhook, adminUi])
  return [receipt]
})
