// Version graph. The current version must be listed first; older versions
// we can migrate from go in `other: [...]`. Passed as an InitScript into
// `sdk.setupInit(...)` and `sdk.setupUninit(...)` so StartOS can run the
// correct migrations on install / update / downgrade / restore.

import { VersionGraph } from '@start9labs/start-sdk'
import { v0_1_0 } from './v0.1.0'
import { v0_2_0 } from './v0.2.0'

export const versions = VersionGraph.of({
  current: v0_2_0,
  // Operators on v0.1.0:N can upgrade to the v0.2.0 line via the
  // StartOS marketplace. v0.1.0 stays in `other` for as long as we
  // want that upgrade path supported; once v0.3 ships and we're
  // confident no one is still on the alpha line, this can be
  // pruned.
  other: [v0_1_0],
})
