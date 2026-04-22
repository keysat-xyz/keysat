// Network interfaces exposed by the service.
//
// Two logical interfaces:
//   - `api`     — the REST API that buyers (purchase flow) and licensed
//                 software (validate flow) hit. Must be reachable from
//                 outside the StartOS host if you're selling to the public,
//                 so we expose it on LAN + Tor + optional clearnet.
//   - `webhook` — the BTCPay webhook landing endpoint. Only BTCPay needs to
//                 reach it; same-host LAN is sufficient.
//
// In practice both live on the same HTTP port (8080) because the service
// routes by path. StartOS's interface concept is about *access surfaces*
// and *display grouping*, not separate ports.

import { sdk } from './sdk'

export const setInterfaces = sdk.setupInterfaces(async ({ effects }) => {
  const apiMulti = sdk.MultiHost.of(effects, 'api-multi')
  await apiMulti.bindPort(8080, { protocol: 'http', preferredExternalPort: 443 })

  const api = sdk.createInterface(effects, {
    name: 'Licensing API',
    id: 'api',
    description:
      'REST API for buyers and licensed software. Public-facing: this is ' +
      'the URL you share with customers and bake into your own software ' +
      'builds as the licensing endpoint.',
    type: 'api',
    hasPrimary: true,
    masked: false,
    schemeOverride: null,
    username: null,
    path: '',
    query: {},
  })

  return [await api.export([apiMulti])]
})
