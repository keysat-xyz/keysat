// Version graph. The current version must be listed first; older versions
// we can migrate from go in `other: [...]`. Passed as an InitScript into
// `sdk.setupInit(...)` and `sdk.setupUninit(...)` so StartOS can run the
// correct migrations on install / update / downgrade / restore.

import { VersionGraph } from '@start9labs/start-sdk'
import { v0_1_0 } from './v0.1.0'

export const versions = VersionGraph.of({
  current: v0_1_0,
  other: [],
})
