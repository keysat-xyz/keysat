// Register actions with StartOS.
//
// The StartOS Actions tab is kept intentionally minimal — only the
// four operations that need to happen outside the admin web UI:
//
//   - Set web UI password — needed for password recovery (you can't
//     reset the password from inside the web UI if you can't log in)
//   - Activate Keysat license — first-install bootstrap for paid
//     customers, and recovery if /data/keysat-license.txt gets lost
//   - Show license status — sanity-check the self-license state
//     without logging into the admin UI
//   - Show credentials — find the admin API key on first install,
//     before you've logged into the admin UI for the first time
//
// Everything else — operator name, payment provider connect / activate,
// scoped API keys, products, policies, licenses, codes, machines,
// webhooks, audit log — lives in the embedded admin web UI under the
// Settings tab and the workspace sidebar. The action source files for
// those operations remain in this directory for reference, but they're
// no longer registered as StartOS UI buttons. This keeps the dashboard
// from feeling like an undifferentiated wall of buttons and aligns with
// "everything in one place" — the web UI.

import { sdk } from '../sdk'
import { activateLicense, showLicenseStatus } from './activateLicense'
import { setWebUiPassword } from './setWebUiPassword'
import { showCredentials } from './showCredentials'

export const actions = sdk.Actions.of()
  // First-install / recovery essentials.
  .addAction(setWebUiPassword)
  .addAction(showCredentials)
  // Keysat self-license (Keysat-licenses-Keysat). Required for paid
  // customers to activate their self-license on first install. The
  // license string itself is provided by your seller.
  .addAction(activateLicense)
  .addAction(showLicenseStatus)
