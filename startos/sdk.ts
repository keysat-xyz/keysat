// Re-export of the SDK pre-bound to our manifest. Import `sdk` from here
// everywhere else in `startos/` so every call benefits from the typed
// narrowing of our package-specific manifest.
//
// NOTE: In 0.4.0.x the SDK builder does not take a store — package-local
// persistent state is now expressed through `FileHelper` (see
// `./fileModels/store.ts`). We just bind the manifest here.

import { StartSdk } from '@start9labs/start-sdk'
import { manifest } from './manifest'

export const sdk = StartSdk.of().withManifest(manifest).build(true)
