// StartOS package manifest. Run through `setupManifest()` from the SDK.
//
// NOTE: This service's source code is source-available but not open source.
// The `license` field takes an SPDX identifier, and the actual license text
// must live in a file named `LICENSE` at the package root (start-cli bundles
// it as an ingredient). Since this project ships under a custom license, we
// use the SPDX `LicenseRef-` prefix per the SPDX spec for non-standard
// licenses. The `LICENSE` file at the package root is a copy of
// `../licensing-service/LICENSE`.

import { setupManifest } from '@start9labs/start-sdk'
import { short, long } from './i18n'

export const manifest = setupManifest({
  id: 'keysat',
  title: 'Keysat Licensing',
  license: 'LicenseRef-Keysat-1.0',
  // packageRepo (the s9pk wrapper source) and upstreamRepo (the daemon source)
  // are the same URL: the StartOS wrapper and the Rust daemon share one monorepo.
  packageRepo: 'https://github.com/keysat-xyz/keysat',
  upstreamRepo: 'https://github.com/keysat-xyz/keysat',
  marketingUrl: 'https://keysat.xyz',
  donationUrl: null,
  docsUrls: [
    'https://github.com/keysat-xyz/keysat/blob/main/README.md',
    'https://github.com/keysat-xyz/keysat/blob/main/KEYSAT_INTEGRATION.md',
  ],
  description: { short, long },
  // A single data volume holds the SQLite database (which in turn holds the
  // server signing key). StartOS encrypts and backs this up automatically.
  volumes: ['main'],
  images: {
    main: {
      // Built from the project's Dockerfile. Build context is this package
      // directory itself (the start-cli default). The Rust source is
      // exposed inside the package dir as `licensing-service/`, which is
      // a symlink to the sibling `../licensing-service/` repo so the
      // upstream sources stay in their natural location while the build
      // context stays self-contained.
      source: {
        dockerBuild: {},
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
    // DepInfo = { description, optional } & ({ metadata: {title, icon} } | { s9pk })
    // We use the s9pk form with `null` since we don't want to bundle a copy of
    // BTCPay's s9pk into our package just to extract its metadata at build time
    // — StartOS will pull the metadata from the installed instance at runtime.
    btcpayserver: {
      description: {
        en_US:
          'Required to receive Bitcoin payments and confirm settlement via webhook.',
      },
      optional: false,
      s9pk: null,
    },
  },
})
