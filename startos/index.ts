// StartOS entry point. Composes every module together so `start-cli` can
// pack the package and so StartOS can find the expected exports.
//
// The ABI StartOS expects (see ExpectedExports in the SDK):
//   - manifest
//   - main
//   - init
//   - uninit
//   - createBackup
//   - actions
//
// In SDK 0.4.0 `setupInit(...inits)` / `setupUninit(...uninits)` are variadic
// — each argument is either an InitScript/UninitScript or an
// InitFn/UninitFn. They run in the order provided.
//
// Ordering of init scripts matters:
//   1. restoreInit   — repopulates the main volume from backup if applicable
//   2. versions      — runs any pending migrations from the version graph
//   3. initFn        — our own first-boot key generation
//   4. setDependencies — publishes our declared dependency on BTCPay
//   5. setInterfaces   — publishes the public-facing API + webhook URL
//   6. actions         — registers the admin actions with StartOS

import { buildManifest } from '@start9labs/start-sdk'
import { sdk } from './sdk'

import { actions } from './actions'
import { createBackup, restoreInit } from './backups'
import { setDependencies } from './dependencies'
import { initFn, uninitFn } from './init'
import { setInterfaces } from './interfaces'
import { main } from './main'
import { manifest as sdkManifest } from './manifest'
import { versions } from './versions'

// `setupManifest(...)` in `./manifest` produces the raw SDKManifest.
// `buildManifest(versions, sdkManifest)` injects `version`, `sdkVersion`,
// `releaseNotes`, `canMigrateTo/From`, normalized `alerts`, `images`
// defaults, etc — producing the final T.Manifest that `start-cli s9pk pack`
// serializes. Exporting the raw SDKManifest here (without buildManifest)
// causes start-cli to fail with: `Deserialization Error: missing field
// `version``.
export const manifest = buildManifest(versions, sdkManifest)

export const init = sdk.setupInit(
  restoreInit,
  versions,
  initFn,
  setDependencies,
  setInterfaces,
  actions,
)

export const uninit = sdk.setupUninit(versions, uninitFn)

export { main, actions, createBackup }
