// StartOS package manifest. Run through `setupManifest()` from the SDK.
//
// NOTE: This service's source code is source-available but not open source
// (see ../../../licensing-service/LICENSE). The `license` field here is
// set to 'Proprietary' accordingly — StartOS displays this on the install
// page so users know what they're installing.

import { setupManifest } from '@start9labs/start-sdk'
import { short, long } from './i18n'

export const manifest = setupManifest({
  id: 'keysat',
  title: 'Keysat',
  license: 'Proprietary',
  packageRepo: 'https://github.com/ten31/keysat-startos',
  upstreamRepo: 'https://github.com/ten31/keysat',
  marketingUrl: 'https://ten31.xyz/keysat',
  donationUrl: null,
  docsUrls: [
    'https://github.com/ten31/keysat/blob/main/README.md',
    'https://github.com/ten31/keysat/blob/main/docs/INTEGRATION.md',
  ],
  description: { short, long },
  // A single data volume holds the SQLite database (which in turn holds the
  // server signing key). StartOS encrypts and backs this up automatically.
  volumes: ['main'],
  images: {
    main: {
      // Built from the project's Dockerfile. The build context is the parent
      // `Licensing/` directory so the Dockerfile can COPY from the sibling
      // `licensing-service/` Rust source; a top-level .dockerignore keeps the
      // uploaded context small.
      source: {
        dockerBuild: {
          workdir: '..',
          dockerfile: 'licensing-service-startos/Dockerfile',
        },
      },
      arch: ['x86_64', 'aarch64'],
    },
  },
  alerts: {
    install: null,
    update: null,
    uninstall: {
      en_US:
        'Uninstalling will delete your server signing key and all license ' +
        'records. Previously-issued license keys will no longer validate ' +
        'against this server. Back up first if you plan to reinstall.',
    },
    restore: null,
    start: null,
    stop: null,
  },
  dependencies: {
    btcpayserver: {
      description: 'Required to receive Bitcoin payments and confirm settlement via webhook.',
      optional: false,
      metadata: {
        title: 'BTCPay Server',
      },
    },
  },
})
