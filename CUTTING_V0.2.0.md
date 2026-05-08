# Cutting v0.2.0:0

The v0.2.0 milestone version is drafted at `startos/versions/v0.2.0.ts`
but **not yet wired in as the current version**. This file documents
exactly what to do when you're ready to flip the switch.

## Pre-flight (do these once, before the cut)

1. **Read through `startos/versions/v0.2.0.ts`** — especially the
   release notes. The notes ship to every operator who installs or
   upgrades; treat them as the public-facing changelog. Edit freely.
2. **Sanity-check the SPA** at `/admin/` on a running `:46` daemon.
   The v0.2 cut is the one where the SPA is "officially" the primary
   interface; if anything's still rough, fix it on the alpha line first.
3. **Confirm no schema changes are pending.** v0.2.0:0 is a label
   change, not a data migration — `licensing-service/migrations/`
   should still end at `0009`. (When v0.3 ships its first schema
   change, that's a `0010_*.sql` file and the migration regression
   tests in `tests/migrations.rs` will run against it automatically.)

## The cut itself (≈5 minutes)

### Step 1 — Wire v0.2.0 in as the current version

Edit `startos/versions/index.ts`:

```ts
import { v0_1_0 } from './v0.1.0'
import { v0_2_0 } from './v0.2.0'   // ← add

export const versions = VersionGraph.of({
  current: v0_2_0,                  // ← change from v0_1_0
  other: [v0_1_0],                  // ← add so 0.1.0:N can upgrade
})
```

### Step 2 — Type-check + build

```bash
cd licensing-service-startos
npm run check         # tsc --noEmit; should pass
make x86              # produces keysat_x86_64.s9pk for v0.2.0:0
```

If the SDK's `VersionInfo.of` signature wants a migration callback
for the upgrade from v0.1.0 → v0.2.0, the `tsc` step will tell you.
The current draft has no migration callback because there's no on-
disk transformation needed — but if `start-sdk` enforces one, add an
empty one:

```ts
export const v0_2_0 = VersionInfo.of({
  version: '0.2.0:0',
  releaseNotes: [...],
  migrations: {
    up: async () => { /* no-op */ },
    down: async () => { /* no-op */ },
  },
})
```

### Step 3 — Publish

```bash
~/.keysat/publish.sh
```

The publish script's gate (current version differs from
`~/.keysat/last_published_version`) will fire because `0.2.0:0` is a
new version string. The script handles upload + registry-add as
usual.

### Step 4 — Verify the upgrade dialog

Refresh the StartOS marketplace on a test instance running
v0.1.0:46 (or any v0.1.0:N). It should now show v0.2.0:0 as
available with the release notes from `v0.2.0.ts` rendered. Click
"Update" and confirm the daemon comes up cleanly post-upgrade.

If the test instance gets stuck (StartOS won't compute the upgrade
graph, daemon panics post-upgrade, anything weird): the v0.2.0:0
.s9pk is still in the registry but you can pull it via
`start-cli registry package remove keysat 0.2.0:0` and roll back to
the alpha line by reverting `versions/index.ts`.

## Rollback

If it goes sideways:

```bash
# Revert versions/index.ts to use v0_1_0 as current
git checkout HEAD~1 -- startos/versions/index.ts

# Bump to a fresh alpha-iteration revision (so the registry has
# something newer than the busted 0.2.0:0)
# Edit startos/versions/v0.1.0.ts → version: '0.1.0:47'
# with release notes explaining the rollback.

# Build + publish
make x86
~/.keysat/publish.sh
```

The bad v0.2.0:0 stays in the registry but operators on
v0.1.0:46 won't see it as the latest if a newer v0.1.0:47 is
present (StartOS picks the highest-version compatible release).

## Why v0.2.0:0 (not v0.2)

The version string is ExVer (`<upstream>:<downstream>`). `0.2.0` is
the upstream milestone; `:0` is the wrapper revision. The next
routine wrapper change on the v0.2 line is `0.2.0:1`. v0.2's first
schema change is a new SQL migration file — the upstream version
doesn't move for that.

The upstream version `0.3.0` opens when we ship a substantial
feature set (Zaprite, recurring subscriptions, tier upgrades, etc.)
that warrants the marketing distinction.
