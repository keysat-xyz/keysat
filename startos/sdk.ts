// Re-export of the SDK pre-bound to our manifest and file models. Import
// `sdk` from here everywhere else in `startos/` so every call benefits from
// the typed narrowing of our package-specific store shape.

import { StartSdk } from '@start9labs/start-sdk'
import { manifest } from './manifest'
import { store } from './fileModels/store'

export const sdk = StartSdk.of().withManifest(manifest).withStore(store).build(true)
