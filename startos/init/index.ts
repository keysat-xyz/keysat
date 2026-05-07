// First-boot initialization.
//
// On fresh install:
//   - Generate an admin API key (stored in the StartOS package-local store;
//     user can retrieve it via the `showCredentials` action if they need to
//     script against the API).
//   - Surface "Connect BTCPay" as a critical task so the operator sees a
//     clear "do this next" prompt in the StartOS dashboard. Cleared by the
//     btcpayStatus action once BTCPay reports connected (see
//     ../actions/configureBtcpay.ts).
//
// The BTCPay webhook secret is no longer stored here — the daemon generates
// and persists it in its own DB during the one-click "Connect BTCPay" flow.
// The field is kept in the store shape for backward compatibility with
// installs made before the authorize flow; it is not authoritative.
//
// On subsequent boots this is a no-op (keys already exist).
//
// SDK 0.4.0 note: InitFn signature is `(effects, kind)` positional — NOT the
// 0.3.x `({effects})` object destructure. `setupOnInit` wraps the function
// into an InitScript so it can be composed with `setupInit(...)`.

import { sdk } from '../sdk'
import { store } from '../fileModels/store'
import { generateSecret } from '../utils'
import { configureBtcpay } from '../actions/configureBtcpay'

/** Replay id used to dedupe + later-clear the BTCPay setup task. */
export const BTCPAY_SETUP_TASK_ID = 'btcpay-initial-setup'

export const initFn = sdk.setupOnInit(async (effects, kind) => {
  const current = await store.read().once()

  if (!current || current.schema_version === 0 || current.schema_version === undefined) {
    await store.write(effects, {
      admin_api_key: current?.admin_api_key || generateSecret(32),
      // Kept in the shape for backcompat; no longer authoritative.
      btcpay_webhook_secret: current?.btcpay_webhook_secret || '',
      operator_name: current?.operator_name || '',
      schema_version: 1,
    })
  }

  // Surface BTCPay setup as a prominent task on first install and on
  // restore (a backup older than the BTCPay-authorize flow may not have a
  // valid BTCPay config). On regular updates / container rebuilds we
  // skip — BTCPay should already be connected by then. createOwnTask is
  // idempotent on the same replayId, so a re-run won't duplicate.
  //
  // Severity is 'important', not 'critical', because 'critical' blocks
  // the service from STARTING until the task is completed — but the
  // configureBtcpay action requires the service to BE running (it makes
  // an HTTP call to the local daemon to kick off the authorize flow).
  // 'critical' would deadlock: task blocks start, action needs running.
  // 'important' shows the task prominently without blocking startup.
  if (kind === 'install' || kind === 'restore') {
    try {
      await sdk.action.createOwnTask(effects, configureBtcpay, 'important', {
        replayId: BTCPAY_SETUP_TASK_ID,
        reason:
          'Connect Keysat to your BTCPay Server to start selling licenses. ' +
          'Your BTCPay instance on this Start9 is already a declared ' +
          'dependency — Keysat just needs to authorize against it.',
      })
    } catch (e) {
      // Don't block init on a task-create failure. Operators can still
      // run "Connect BTCPay" manually.
      // eslint-disable-next-line no-console
      console.warn('createOwnTask(configureBtcpay) failed:', e)
    }
  }
})

export const uninitFn = sdk.setupOnUninit(async (_effects, _target) => {
  // Nothing to tear down at the StartOS level — the DB volume is handled by
  // StartOS directly when the package is uninstalled.
})
