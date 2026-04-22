// Declare our dependency on BTCPay Server. StartOS uses this to:
//   - prevent starting if BTCPay isn't installed,
//   - gate our service's health status on BTCPay's,
//   - provide the `btcpayserver.startos` hostname inside our container.

import { sdk } from './sdk'

export const setDependencies = sdk.setupDependencies(async ({ effects }) => {
  return {
    btcpayserver: {
      kind: 'running',
      versionRange: '>=1.11.0',
      healthChecks: [],
    },
  }
})
