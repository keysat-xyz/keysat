// Declare our dependency on BTCPay Server. StartOS uses this to:
//   - prevent starting if BTCPay isn't installed,
//   - gate our service's health status on BTCPay's,
//   - provide the `btcpayserver.startos` hostname inside our container.
//
// versionRange uses ExVer (StartOS's Extended Versioning). The ':0' suffix
// is the downstream revision; ':0' is the conventional value meaning "any
// downstream revision of upstream version 1.11.0 or later".

import { sdk } from './sdk'

export const setDependencies = sdk.setupDependencies(async ({ effects: _effects }) => {
  return {
    btcpayserver: {
      kind: 'running',
      versionRange: '>=1.11.0:0',
      healthChecks: [],
    },
  }
})
