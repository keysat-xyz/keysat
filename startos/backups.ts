// Backup & restore.
//
// Everything important lives in the `main` volume (SQLite DB, which in turn
// contains the signing key). StartOS's default backup mechanism captures
// the whole volume, so we don't need custom backup logic — we just opt in.
//
// `setupBackups` returns `{ createBackup, restoreInit }`. `createBackup` is
// the package-level backup export; `restoreInit` is an InitScript we chain
// into `sdk.setupInit(...)` so that a restore triggers the right init
// sequence after the volume is repopulated.
//
// NOTE: The JSDoc example in 0.4.0 shows `sdk.Backups.volumes('main')`, but
// the actual runtime/type name is `ofVolumes`. The example is stale.

import { sdk } from './sdk'

export const { createBackup, restoreInit } = sdk.setupBackups(
  async () => sdk.Backups.ofVolumes('main'),
)
