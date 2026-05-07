// Daemon definition — the thing that actually runs when the service is
// started. Passes configuration into the Rust binary via environment
// variables (same interface as `.env.example` in the upstream project).
//
// SDK 0.4.0 shape:
//   - `setupMain(async ({ effects }) => Daemons)` — no `started` any more.
//   - Mounts are built via `sdk.Mounts.of().mountVolume(...)` (immutable
//     builder) and passed as a single object to `sdk.SubContainer.of`.
//   - Daemons are created via `sdk.Daemons.of(effects)` (effects directly).
//   - Store reads use the FileHelper reactive API: `.read().const(effects)`
//     so the daemon re-runs if the store changes at runtime.
//   - The public URL is read from our own `api` service interface via
//     `sdk.serviceInterface.getOwn(...).const()` + `.addressInfo.nonLocal.format()`.

import { sdk } from './sdk'
import { store } from './fileModels/store'

/**
 * Pick a URL from a service interface's address list that's actually
 * reachable from the operator's normal-LAN browser.
 *
 * StartOS hands us a list of URLs the service is reachable on, but they
 * vary in who-can-reach-them:
 *   - mDNS `.local` hostname        → reachable on the operator's LAN
 *   - LAN RFC1918 IP (192.168, etc) → reachable on the operator's LAN
 *   - public clearnet URL           → reachable from anywhere
 *   - StartTunnel local IP (10.59)  → only reachable inside StartOS
 *   - .startos bridge hostname      → only reachable inside containers
 *   - localhost / 127.x             → only reachable inside the container
 *
 * Naively picking `addressInfo.nonLocal.format()[0]` can land on the
 * StartTunnel-local IP, which breaks any flow where the operator's
 * browser actually has to follow the URL. This helper ranks URLs by
 * realistic browser reachability instead.
 */
// Shared URL filters used by both pickers below.
function isLocalhost(u: string): boolean {
  return (
    u.startsWith('http://localhost') ||
    u.startsWith('https://localhost') ||
    u.startsWith('http://127.') ||
    u.startsWith('https://127.')
  )
}
function isBridge(u: string): boolean {
  return u.includes('.startos:')
}
function isMdns(u: string): boolean {
  return /\/\/[^/:]+\.local(:|\/)/.test(u)
}
function isRfc1918(u: string): boolean {
  return (
    /\/\/192\.168\.\d+\.\d+(:|\/)/.test(u) ||
    /\/\/172\.(1[6-9]|2\d|3[01])\.\d+\.\d+(:|\/)/.test(u) ||
    // Real RFC1918 10.0.0.0/8 — but exclude StartTunnel's 10.59.x.x range
    // which is StartOS-internal and not reachable from a normal browser.
    (/\/\/10\.\d+\.\d+\.\d+(:|\/)/.test(u) && !/\/\/10\.59\./.test(u))
  )
}
function isStarttunnelLocal(u: string): boolean {
  return /\/\/10\.59\./.test(u)
}
function isIpv4(u: string): boolean {
  return /\/\/\d+\.\d+\.\d+\.\d+(:|\/)/.test(u)
}
function isIpv6Bracketed(u: string): boolean {
  return /\/\/\[/.test(u)
}

/// Pick a URL the OPERATOR's browser can reach during one-time setup
/// flows (OAuth authorize, etc.). Operator is typically on the same
/// LAN as the Start9, so mDNS / RFC1918 LAN URLs are preferred —
/// they're faster and don't depend on Cloudflare being up.
function pickBrowserUrl(
  allUrls: string[],
  addrInfo?: { nonLocal: { format(): string[] } } | null | undefined,
): string | undefined {
  const browserUsable = (u: string) =>
    !isLocalhost(u) && !isBridge(u) && !isStarttunnelLocal(u)

  const mdnsUrls = allUrls.filter((u) => isMdns(u) && browserUsable(u))
  if (mdnsUrls.length > 0) return mdnsUrls[0]
  const lanUrls = allUrls.filter((u) => isRfc1918(u) && browserUsable(u))
  if (lanUrls.length > 0) return lanUrls[0]
  const nonLocalUrls = (addrInfo?.nonLocal.format() ?? []).filter(browserUsable)
  if (nonLocalUrls.length > 0) return nonLocalUrls[0]
  const anyUsable = allUrls.filter(browserUsable)
  if (anyUsable.length > 0) return anyUsable[0]
  return undefined
}

/// Pick a URL that BUYERS on the public internet can reach. Used to
/// rewrite checkout URLs so they're browser-reachable from anywhere.
/// Prefers domain-named URLs (clearnet via StartTunnel) over IP/mDNS
/// addresses. Falls back to LAN/mDNS only if no public domain is set
/// up — useful for local testing but won't work for real customers.
function pickPublicUrl(allUrls: string[]): string | undefined {
  const usable = allUrls.filter(
    (u) => !isLocalhost(u) && !isBridge(u) && !isStarttunnelLocal(u),
  )
  // Prefer URLs with a real domain name (no IP, no .local).
  const clearnet = usable.filter(
    (u) => !isIpv4(u) && !isIpv6Bracketed(u) && !isMdns(u),
  )
  if (clearnet.length > 0) return clearnet[0]
  // Fall back to LAN (still browser-reachable for testing on the same network).
  const mdns = usable.filter(isMdns)
  if (mdns.length > 0) return mdns[0]
  const lan = usable.filter(isRfc1918)
  if (lan.length > 0) return lan[0]
  return usable[0]
}

export const main = sdk.setupMain(async ({ effects }) => {
  const storeData = await store.read().const(effects)
  if (!storeData) {
    // Init should always run before main, so this is a real error.
    throw new Error(
      'Keysat store.json is missing — init did not run. Try restarting the service.',
    )
  }

  // Public URL advertised to buyers / baked into webhook payloads. We read
  // our own `api` interface from StartOS at runtime so this works whether the
  // operator exposes on Tor, LAN, or clearnet. `.nonLocal` filters out
  // localhost/link-local; we pick the first resulting URL, falling back to
  // localhost only if StartOS hasn't filled in the interface yet.
  // Pick a browser-reachable URL for ourselves. This is what we hand to
  // BTCPay as the OAuth redirect_uri (the operator's browser follows it
  // after clicking Authorize), and it's the URL buyers later use to
  // poll purchase status. Same ranking logic as for BTCPay's URL —
  // prefer mDNS .local and RFC1918 LAN IPs, deprioritize StartTunnel
  // local addresses (10.59.x.x), avoid localhost / bridge.
  const iface = await sdk.serviceInterface.getOwn(effects, 'api').const()
  const ownAllUrls = iface?.addressInfo?.format() ?? []
  // Use the PUBLIC-preferred picker for our own URL — buyers redirected
  // back from BTCPay after payment hit this URL with their browser; it
  // needs to be clearnet-resolvable. Falls back to the operator-facing
  // mDNS/LAN URL if no clearnet domain is set up.
  const publicUrl =
    pickPublicUrl(ownAllUrls) ??
    pickBrowserUrl(ownAllUrls, iface?.addressInfo) ??
    'http://localhost:8080'

  // BTCPay's PUBLIC web UI URL — distinct from the internal-network
  // hostname we use for daemon-to-daemon API calls. The operator's
  // browser is redirected here to authorize Keysat against BTCPay; that
  // means the URL must be resolvable from a normal browser.
  //
  // We can't hardcode BTCPay's interface ID because it's package-
  // specific (and the previous version of this code guessed wrong by
  // assuming `'ui'`). Instead, fetch ALL interfaces BTCPay exposes,
  // pick the one whose TYPE is `'ui'`, and read its address list.
  // Within that, prefer non-local URLs but accept LAN URLs as a
  // fallback (they're perfectly browser-reachable for the operator).
  const btcpayIfaces = await sdk.serviceInterface
    .getAll(effects, { packageId: 'btcpayserver' })
    .const()
  const ifaceList = btcpayIfaces ?? []
  const uiIface = ifaceList.find((i) => i.type === 'ui') ?? null
  const btcpayAllUrls = uiIface?.addressInfo?.format() ?? []
  const btcpayBrowserUrl = pickBrowserUrl(btcpayAllUrls, uiIface?.addressInfo) ?? ''
  // PUBLIC URL preference is different — for buyer-facing checkout
  // URLs we want a clearnet domain that random internet customers
  // can resolve. Falls back to the operator-facing browser URL (mDNS/
  // LAN) if no clearnet domain is set up; that's only useful for
  // local testing but won't break production.
  const btcpayPublicUrl = pickPublicUrl(btcpayAllUrls) ?? btcpayBrowserUrl
  console.info(
    `Keysat BTCPay lookup: ${ifaceList.length} interface(s) declared by btcpayserver. ` +
      `Types found: [${ifaceList.map((i) => `${i.id}:${i.type}`).join(', ')}]. ` +
      `Selected ui interface id="${uiIface?.id ?? '(none)'}". ` +
      `Picked browser URL "${btcpayBrowserUrl || '(none)'}". ` +
      `Picked public URL "${btcpayPublicUrl || '(none — falling back to internal URL)'}".`,
  )

  const mounts = sdk.Mounts.of().mountVolume({
    volumeId: 'main',
    mountpoint: '/data',
    subpath: null,
    readonly: false,
  })

  const sub = await sdk.SubContainer.of(
    effects,
    { imageId: 'main' },
    mounts,
    'keysat',
  )

  return sdk.Daemons.of(effects).addDaemon('primary', {
    subcontainer: sub,
    exec: {
      // Use the Dockerfile's ENTRYPOINT / CMD instead of hardcoding a command
      // here; the image is the source of truth for how to launch the binary.
      command: sdk.useEntrypoint(),
      env: {
        KEYSAT_BIND: '0.0.0.0:8080',
        KEYSAT_DB_PATH: '/data/keysat.db',
        KEYSAT_PUBLIC_URL: publicUrl,
        KEYSAT_ADMIN_API_KEY: storeData.admin_api_key,
        KEYSAT_OPERATOR_NAME: storeData.operator_name,
        // Reachable because of our dependency on btcpayserver. This is
        // the INTERNAL hostname used for daemon-to-daemon API calls.
        // Keysat's container can't reliably reach the public StartTunnel
        // URL from outside (egress is restricted), so all BTCPay API
        // traffic stays on the local Docker network — fast + always
        // reachable. The downside (BTCPay returns checkout URLs with
        // this internal hostname) is mitigated in the daemon: we
        // rewrite the host of every checkout URL to the public
        // BTCPAY_BROWSER_URL before handing it back to a buyer.
        BTCPAY_URL: 'http://btcpayserver.startos:23000',
        // BTCPay's web UI URL for OPERATOR-facing browser redirects
        // (the OAuth-style authorize flow). Operator is on the same
        // LAN as the Start9 typically, so this prefers mDNS / LAN.
        BTCPAY_BROWSER_URL: btcpayBrowserUrl,
        // BTCPay's PUBLIC URL for BUYER-facing redirects. Used by the
        // daemon to rewrite checkout URLs returned by BTCPay so they
        // resolve from random internet browsers. Prefers clearnet
        // domain names (e.g. `https://btcpay.your-domain.com`); falls
        // back to LAN/mDNS only if no public domain is set up. If
        // empty, daemon won't rewrite (only useful for local testing).
        BTCPAY_PUBLIC_URL: btcpayPublicUrl,
        // The three credentials below are left empty in the normal case —
        // the daemon persists them in its own DB after the one-click
        // "Connect BTCPay" action completes. Only seed them here if you are
        // migrating from a pre-authorize-flow install.
        BTCPAY_API_KEY: '',
        BTCPAY_STORE_ID: '',
        BTCPAY_WEBHOOK_SECRET: '',
        RUST_LOG: 'info,sqlx=warn,hyper=warn',
      },
    },
    ready: {
      display: 'API',
      fn: () =>
        sdk.healthCheck.checkPortListening(effects, 8080, {
          successMessage: 'Keysat API is accepting requests',
          errorMessage: 'Keysat API is not responding on port 8080',
        }),
    },
    requires: [],
  })
})
