// Backup & restore.
//
// Everything important lives in the `main` volume (SQLite DB, which in turn
// contains the signing key). StartOS's default backup mechanism captures
// the whole volume, so we don't need custom backup logic — we just opt in.

import { sdk } from './sdk'

export const { createBackup, restoreBackup } = sdk.setupBackups(async ({ effects }) => [
  sdk.Backups.volumes('main'),
])
