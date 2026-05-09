// Register actions with StartOS.
//
// As of v0.1.0:11 the StartOS Actions tab is intentionally minimal —
// only setup-time operations live here:
//
//   - General        → Set operator name
//   - BTCPay         → Connect / Check / Disconnect
//   - License        → Activate Keysat license / Show license status
//   - Credentials    → Show admin API key
//
// Everything else (products, policies, discount codes, licenses,
// machines, webhooks, audit log) lives in the embedded admin web UI
// at /admin/. The action source files remain in this directory for
// reference — and the underlying admin HTTP API is unchanged — but
// they're no longer registered as StartOS UI buttons. This keeps the
// dashboard from feeling like an undifferentiated wall of buttons.
//
// The web UI uses the same /v1/admin/* endpoints those actions used to
// call, so functionality is identical; only the UI surface changed.

import { sdk } from '../sdk'
import { activateLicense, showLicenseStatus } from './activateLicense'
import { switchPaymentProvider } from './activatePaymentProvider'
import { btcpayStatus, configureBtcpay, disconnectBtcpay } from './configureBtcpay'
import {
  configureZaprite,
  disconnectZaprite,
  showZapriteWebhookSetup,
  zapriteStatus,
} from './configureZaprite'
import { setOperatorName } from './setOperatorName'
import { setWebUiPassword } from './setWebUiPassword'
import { showCredentials } from './showCredentials'

export const actions = sdk.Actions.of()
  // General
  .addAction(setOperatorName)
  .addAction(setWebUiPassword)
  // BTCPay setup (Bitcoin-only payments via your own BTCPay Server)
  .addAction(configureBtcpay)
  .addAction(btcpayStatus)
  .addAction(disconnectBtcpay)
  // Zaprite setup (Bitcoin + fiat-card payments via Zaprite's broker)
  .addAction(configureZaprite)
  .addAction(zapriteStatus)
  .addAction(showZapriteWebhookSetup)
  .addAction(disconnectZaprite)
  // Single unified switch action — flips active provider via a
  // dropdown so operators don't see two confusing "Activate X"
  // actions side-by-side, each appearing to override the other.
  .addAction(switchPaymentProvider)
  // Keysat self-license (Keysat-licenses-Keysat)
  .addAction(activateLicense)
  .addAction(showLicenseStatus)
  // Credentials
  .addAction(showCredentials)
