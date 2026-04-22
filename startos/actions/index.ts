// Register every action with StartOS.

import { sdk } from '../sdk'
import { btcpayStatus, configureBtcpay } from './configureBtcpay'
import { createPolicy } from './createPolicy'
import { createProduct } from './createProduct'
import { deactivateMachine } from './deactivateMachine'
import { issueLicense } from './issueLicense'
import { listMachines } from './listMachines'
import { listWebhooks } from './listWebhooks'
import { registerWebhook } from './registerWebhook'
import { revokeLicense } from './revokeLicense'
import { searchLicenses } from './searchLicenses'
import { setOperatorName } from './setOperatorName'
import { showCredentials } from './showCredentials'
import { suspendLicense } from './suspendLicense'
import { unsuspendLicense } from './unsuspendLicense'
import { viewAuditLog } from './viewAuditLog'

export const actions = sdk.Actions.of()
  // General
  .addAction(setOperatorName)
  // BTCPay
  .addAction(configureBtcpay)
  .addAction(btcpayStatus)
  // Credentials
  .addAction(showCredentials)
  // Products + Policies
  .addAction(createProduct)
  .addAction(createPolicy)
  // Licenses
  .addAction(issueLicense)
  .addAction(searchLicenses)
  .addAction(suspendLicense)
  .addAction(unsuspendLicense)
  .addAction(revokeLicense)
  // Machines
  .addAction(listMachines)
  .addAction(deactivateMachine)
  // Webhooks
  .addAction(registerWebhook)
  .addAction(listWebhooks)
  // Diagnostics
  .addAction(viewAuditLog)
