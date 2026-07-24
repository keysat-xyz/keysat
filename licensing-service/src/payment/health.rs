//! Per-provider API-authentication health — the decision layer behind the
//! operator failure alerts.
//!
//! Keysat can lose the ability to sell without anyone noticing: if the
//! operator's BTCPay or Zaprite API key is revoked, nothing surfaces it until
//! a buyer reaches checkout and cannot pay. [`ProviderAuthHealth`] is what
//! notices. It folds a stream of observed provider-call outcomes into the two
//! questions an operator cares about — "does this key still authenticate?" and
//! "can it still create an invoice?"
//!
//! **Those are two questions, not one, and this module keeps them apart.** A
//! key can authenticate perfectly and still be unable to sell, because
//! different calls need *different permissions* (BTCPay's
//! `canviewstoresettings` for the liveness probe, `canviewinvoices` for the
//! reconcile sweep, `cancreateinvoice` for a checkout). So there are two
//! independent [`FailureStreak`]s, each with its own count, its own start
//! timestamp and its own alert: [`auth_dead`](ProviderAuthHealth::auth_dead)
//! and [`cannot_sell`](ProviderAuthHealth::cannot_sell). Merging them back into
//! one counter is the bug this split exists to prevent — see the design note on
//! [`ProviderAuthHealth`].
//!
//! Which streak a given call touches is decided by its **call-site label**, and
//! labels fall into three groups, not two: [`is_probe_label`] (liveness),
//! [`is_sell_label`] (created an invoice), and everything else — reads, contact
//! writes, tip payouts — which is the largest group and clears nothing but
//! `auth_dead`.
//!
//! Two layers live here, and that split is the point too.
//!
//! [`ProviderAuthHealth`] is the **rule**, and it is pure: no IO, no lock, no
//! ambient clock (every entry point takes `now`). It can be reasoned about, and
//! tested, one call at a time.
//!
//! [`ProviderHealthSink`] over [`ProviderHealthMap`] is the **boundary** that
//! makes the rule usable from many providers and many concurrent requests. It
//! owns the one thing the rule deliberately does not: the lock, held once
//! across each read-modify-write. Everything shared and everything fallible
//! sits there, so nothing above it has to think about either.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Consecutive counting failures a **single streak** must reach before it can
/// alert.
///
/// The count arm is what kills a fluke — one 401 from a provider having a bad
/// minute is not a revoked key. Both streaks apply it independently, so two
/// failures of each kind is still two of each, never three of "something".
pub const ALERT_MIN_CONSECUTIVE_FAILURES: u32 = 3;

/// How long a streak must already have been running before it can alert.
///
/// The wall-clock arm is what kills a key rotation. Without it, an operator who
/// revokes the old key and pastes the new one thirty seconds later gets paged
/// for their own maintenance, because three retries can easily land inside that
/// window.
pub const ALERT_MIN_STREAK: Duration = Duration::from_secs(10 * 60);

/// Call label reserved for the BTCPay liveness probe.
///
/// The probe does not exist yet. This constant does, so that the probe is
/// written against the classification rule instead of the rule being retrofitted
/// to whatever string the probe happened to pass. See [`is_probe_label`].
pub const BTCPAY_PROBE_LABEL: &str = "btcpay.probe_auth";

/// Call label reserved for the Zaprite liveness probe.
///
/// Deliberately **not** `"zaprite.ping"`. `ZapriteClient::ping` already carries
/// that label on behalf of a different caller — the operator's Connect
/// validation in `api/zaprite_authorize.rs` — and a 403 there is a real
/// "your key cannot do this" signal that must keep counting.
///
/// So the probe may reuse `ping`'s HTTP call but **not** `ping` as written:
/// the label is hard-coded at that call site, and nothing here can bind it.
/// The label's type is `&'static str`, so the compiler will not catch a slip
/// either — the probe has to thread this constant through explicitly. Getting
/// it wrong breaks the rule in **both** directions:
/// - every probe **403** would file under `"zaprite.ping"`, which is not a
///   probe label, so it would count and the daemon would alert on a key that
///   sells perfectly well; and
/// - every probe **success** would count as a hot-path success and clear
///   [`cannot_sell`](ProviderAuthHealth::cannot_sell) every 15 minutes,
///   silently reopening the exact hole the split counters exist to close.
///
/// Step 3b should therefore assert end-to-end that a probe success leaves
/// `cannot_sell` standing, not merely that the constant is passed.
pub const ZAPRITE_PROBE_LABEL: &str = "zaprite.probe_auth";

/// The complete set of labels treated as liveness probes.
const PROBE_LABELS: &[&str] = &[BTCPAY_PROBE_LABEL, ZAPRITE_PROBE_LABEL];

/// The complete set of labels whose **success** proves an invoice can still be
/// created — the only successes that clear
/// [`cannot_sell`](ProviderAuthHealth::cannot_sell).
///
/// An **allow-list, not a deny-list**, and that direction is the whole point.
/// A new call added later does not silently join this set, so it cannot
/// silently start clearing a streak it has not earned the right to clear. See
/// [`is_sell_label`].
const SELL_LABELS: &[&str] = &[
    // The BTCPay checkout call.
    "btcpay.create_invoice",
    // The Zaprite checkout call — an "order" is Zaprite's invoice.
    "zaprite.create_order",
    // `POST /v1/orders/charge`, the recurring-renewal worker's call. It bills
    // an order against a saved payment profile — strictly more than the other
    // two prove, so its 200 is at least as good.
    "zaprite.charge_order_with_profile",
];

/// Is this call label a liveness probe rather than a real piece of work?
///
/// The distinction is real: the probe deliberately touches a broader permission
/// than the sell path — BTCPay's `canviewstoresettings` versus the
/// `cancreateinvoice` a checkout actually needs — so a key that sells perfectly
/// well can 403 on the probe forever.
///
/// The answer decides two things:
/// - a **403** on a probe label does not count (it is a permissions
///   observation, [`ProviderAuthHealth::probe_403_since`]); and
/// - a **success** on a probe label is what *clears*
///   [`probe_403_since`](ProviderAuthHealth::probe_403_since), being the only
///   evidence the broader permission was granted.
///
/// It does **not** decide whether a success clears
/// [`cannot_sell`](ProviderAuthHealth::cannot_sell) — that is
/// [`is_sell_label`]'s job, and the two questions are independent. A label can
/// be neither.
///
/// A **401** is label-blind: it counts toward
/// [`auth_dead`](ProviderAuthHealth::auth_dead) everywhere, and any success
/// anywhere clears that streak.
///
/// **Membership is by call-site label, never by method name.** One HTTP call
/// can be two different things depending on who invoked it, and `ping` is
/// exactly that case: operator Connect validation today, liveness probe once
/// the probe lands. Keying on the method would silently stop counting a genuine
/// Connect-time 403.
///
/// Unknown labels are classified as **not** a probe, and that is the safe
/// default in both directions now that the two classifications are separate. A
/// call added later has its 403s counted, which fails toward a visible,
/// correctable false alarm rather than toward silently losing the signal — and
/// a lone misclassified 403 still cannot fire anything on its own, because
/// alerting needs three of them spread over ten minutes. Its *successes*
/// meanwhile clear nothing but `auth_dead`, because `cannot_sell` is cleared by
/// an allow-list that the new label is not on.
pub fn is_probe_label(label: &str) -> bool {
    PROBE_LABELS.contains(&label)
}

/// Does a success on this call label prove an invoice can still be created?
///
/// Only these successes clear [`cannot_sell`](ProviderAuthHealth::cannot_sell).
/// The governing principle, and it is the same one that split the counters in
/// the first place: **a success clears a streak only if it proves the thing
/// that streak measures.** `cannot_sell` measures "can this key create an
/// invoice", so only a call that actually created one answers it.
///
/// Nothing else qualifies, and the near-misses are worth naming because each
/// one looks like it should:
/// - `btcpay.get_invoice` / `zaprite.get_order` need `canviewinvoices`, not
///   `cancreateinvoice`. This is the case that forced the allow-list:
///   `reconcile.rs` ticks every 60 seconds against every invoice pending under
///   72 hours, so a deny-list would have let reconcile wipe `cannot_sell` about
///   ten times per ten-minute alert window and the streak could never mature.
/// - `zaprite.create_contact` writes a contact record; it creates no invoice.
///   Excluding it costs nothing in practice: `zaprite/provider.rs` only ever
///   calls it as the immediate prerequisite of `create_order`, so any real sale
///   clears the streak a moment later anyway.
/// - `zaprite.ping` is the operator's Connect validation.
/// - `btcpay.pay_lightning_invoice` pays a tip *out*. It needs
///   `canuselightningnode` and is neither probe nor sell — see the note on
///   [`cannot_sell`](ProviderAuthHealth::cannot_sell).
///
/// **Allow-list, never a deny-list.** A call added later has to be added here
/// deliberately, so the failure mode of forgetting is a `cannot_sell` that
/// lingers visibly (a false alarm an operator can see and correct) rather than
/// one that is silently wiped and never alerts. `sell_labels_are_exactly_the_invoice_creating_calls`
/// pins the membership so widening it cannot pass unnoticed.
pub fn is_sell_label(label: &str) -> bool {
    SELL_LABELS.contains(&label)
}

/// One run of consecutive failures, plus the rule that decides when it alerts.
///
/// [`ProviderAuthHealth`] holds two of these. Factoring the run out of that
/// struct is not tidiness: it means the alert rule is written **once**, so the
/// two streaks cannot drift into slightly different thresholds or a
/// slightly different `first_failure_at` discipline as they are extended.
///
/// The fields are `pub` because the health-summary endpoint reports them; the
/// *mutating methods* are private to this module, which is what keeps
/// `first_failure_at` meaning "when this run began" rather than "when it was
/// last extended" for every value this module produces. A caller could still
/// assign the fields directly — see the `None` arm of [`is_alerting`](Self::is_alerting),
/// which exists for exactly that and for rows rehydrated from a future
/// `provider_health` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureStreak {
    /// Length of the current run of failures counted into this streak. Reset
    /// by the successes that clear it; untouched by anything else.
    pub consecutive: u32,
    /// When the current run began — stamped on the 0→1 transition and left
    /// alone while the run extends. `None` whenever `consecutive` is 0.
    pub first_failure_at: Option<SystemTime>,
}

impl FailureStreak {
    /// Extend the run by one, stamping the start on the 0→1 transition only.
    fn count_failure(&mut self, now: SystemTime) {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive == 1 {
            self.first_failure_at = Some(now);
        }
    }

    /// End the run. The next failure starts a new one, timed from itself.
    fn reset(&mut self) {
        self.consecutive = 0;
        self.first_failure_at = None;
    }

    /// Has this run met **both** alert arms?
    ///
    /// A clock that has moved backwards since the run started yields `false`
    /// rather than an arbitrary duration — a system clock correction is not
    /// evidence of a revoked key.
    ///
    /// The mirror case is a known, accepted ceiling: `SystemTime` is
    /// NTP-steppable, so a large forward correction landing inside a young
    /// run can satisfy the wall-clock arm early and let a key rotation alert.
    /// An `Instant` for the run plus a `SystemTime` for display would be
    /// immune, at the cost of two clocks in one struct; a spurious alert
    /// during a clock step is a cheaper failure than the alternative.
    pub fn is_alerting(&self, now: SystemTime) -> bool {
        if self.consecutive < ALERT_MIN_CONSECUTIVE_FAILURES {
            return false;
        }
        match self.first_failure_at {
            Some(started) => now
                .duration_since(started)
                .map(|elapsed| elapsed >= ALERT_MIN_STREAK)
                .unwrap_or(false),
            // Unreachable while this type's own invariant holds: a count of 1
            // or more always carries a `first_failure_at`. The arm exists
            // because both fields are `pub`, so a struct literal built
            // elsewhere — or a row rehydrated from the eventual
            // `provider_health` table — can present a count with no start
            // time. Refusing to alert on a run with no known start is the
            // safe reading of that.
            None => false,
        }
    }
}

/// One payment provider's authentication health, folded from observed calls.
///
/// # The rule
///
/// Two independent streaks, each counting a different failure and cleared by a
/// different success:
///
/// |  | [`auth_dead`](Self::auth_dead) | [`cannot_sell`](Self::cannot_sell) |
/// |---|---|---|
/// | Counts | **401 on any label**, probe or hot path | **403 on a hot-path label only** |
/// | Cleared by | **any success**, on any label | **a [sell-label](is_sell_label) success only** |
/// | Never touched by | a 403 of any kind | a probe 403, and every non-sell success |
///
/// - **401 always counts**, on every label including the probe. Verified
///   2026-07-24 against both providers: a nonexistent key returns 401, and
///   revocation deletes the token record server-side so it takes the same path.
/// - **403 counts on any non-probe label**, into `cannot_sell`. Whatever the
///   underlying cause, the operator cannot sell through that call.
/// - **403 on a probe label never counts** into either streak, and never resets
///   either. It is recorded as [`probe_403_since`](Self::probe_403_since) and
///   reported as a non-alerting permissions observation.
/// - A counting failure extends its own streak and stamps that streak's
///   `first_failure_at` **on the 0→1 transition only**, so the timestamp marks
///   when the run began rather than when it was last extended.
/// - **Any** success, on any label, resets `auth_dead`, sets
///   [`last_success_at`](Self::last_success_at) and clears
///   [`last_status`](Self::last_status). Only a **[sell-label](is_sell_label)**
///   success also resets `cannot_sell`; only a **probe** success clears
///   `probe_403_since`. Those two are independent questions, and most labels
///   answer neither.
/// - Every other **status** — 5xx, 429, 404 — is **inert**: it neither
///   increments nor resets anything. A provider outage must not manufacture an
///   auth alert, and must not paper over one either. So is an outcome that
///   never reached a status at all (a timeout, a DNS or TLS failure), because
///   nothing is recorded for it.
///
///   Note what that does *not* say. Once a status is known the call is
///   recorded, so a 2xx whose body is unreadable or whose JSON does not parse
///   still records a **success**, and a 401 whose body is unreadable still
///   **counts**. Both are deliberate; see the wiring note at the bottom.
/// - **Alerting**, per streak = that streak's count `>= `[`ALERT_MIN_CONSECUTIVE_FAILURES`]
///   **and** it is at least [`ALERT_MIN_STREAK`] old. [`is_alerting`](Self::is_alerting)
///   rolls the two up for an overall status; a consumer that reports *which*
///   condition fired must read each streak, because they do not mean the same
///   thing.
///
/// # Why the counters are split
///
/// An earlier revision kept **one** counter that any success reset. That is
/// wrong the moment a liveness probe exists, and wrong in the direction that
/// loses the signal. The probe and the sell path authenticate against different
/// permissions, so a probe *success* would clear a streak built from hot-path
/// 403s — re-conflating exactly what the probe-403 carve-out separated. With a
/// 15-minute probe cadence against a 10-minute alert window, a scope-broken key
/// (has `canviewstoresettings`, lacks `cancreateinvoice`) sells nothing and yet
/// alerts only if three checkouts 403 inside the ten minutes following a probe
/// success. On a low-traffic instance that may never happen.
///
/// **The governing principle is narrower than "split the counters", and it is
/// what [`is_sell_label`] applies: a success clears a streak only if it proves
/// the thing that streak measures.** The probe is not the only call that fails
/// that test. `reconcile.rs` ticks every 60 seconds and calls `get_invoice` /
/// `get_order` on every invoice pending under 72 hours, and those need
/// `canviewinvoices`, not `cancreateinvoice` — so treating "not a probe" as
/// "proves selling works" would have let reconcile wipe `cannot_sell` roughly
/// ten times per alert window and the streak could never mature. Same
/// never-alerts outcome the split was chosen to prevent, at a 15× tighter
/// cadence.
///
/// **Accepted cost, stated plainly.** `cannot_sell` now clears only on a real
/// sale, so after an operator fixes a scope-broken key it stays red until the
/// next purchase or renewal. That is the safe direction — a visible false
/// alarm the operator can reconcile against their own fix, rather than a
/// silently wiped streak that never alerts — and it does not delay the
/// *authentication* recovery, which `auth_dead` reports from any success
/// including the probe's, every 15 minutes. Step 4a should render
/// `cannot_sell` with its `first_failure_at` so a stale red is legible as
/// "no sale attempted since".
///
/// **Neither streak may be rendered as a flat "key revoked".** `auth_dead`
/// means authentication is failing and the key *may* have been revoked;
/// `cannot_sell` means invoices cannot be created, which on BTCPay is scope,
/// not revocation.
///
/// # Invariant: no in-memory history may gate alerting
///
/// This map is in-memory, and StartOS restarts the daemon on every package
/// update. An earlier revision of this rule gated the 403 handling on a runtime
/// `ever_succeeded` flag; because an already-revoked key can never flip such a
/// flag true, the miss was **permanent** across every future restart. So there
/// is deliberately no field of that shape here, and there must never be one:
/// the rule reads only the current call's label and status. A tracker that has
/// just been created must be able to reach an alert on **either** streak from a
/// cold start, which `fresh_tracker_after_restart_still_alerts` pins.
///
/// The in-memory-ness is itself a documented ceiling — a restart forgets both
/// streaks and the operator waits out another ten minutes — with the upgrade
/// path being a `provider_health` table. It is a delay, not a correctness bug,
/// precisely because of the invariant above.
///
/// # Design decisions this type fixes for its callers
///
/// **1. It lives in its own module (`payment/health.rs`), not in
/// `payment/mod.rs`.** `payment/mod.rs` is the provider trait plus the shared
/// domain vocabulary (`Money`, `Rail`, `ProviderInvoiceStatus`, …) and is
/// already what every provider file imports. The tracker is a self-contained
/// rule with its own constants, its own label vocabulary and a test module
/// several times its own size; folding that into the hub would bury the trait.
/// There is intentionally **no re-export** from `payment/mod.rs`: one type, one
/// path, `crate::payment::health::ProviderAuthHealth`.
///
/// **2. A client holds one pre-bound sink handle, not `(sink, provider_id)`.**
/// That handle is [`ProviderHealthSink`], and `BtcpayClient` / `ZapriteClient`
/// each hold a single `Option<ProviderHealthSink>` attached through a
/// `with_sink()` builder rather than a `new()` parameter, so the test-only
/// `ZapriteClient::new` call sites keep compiling. Two fields would be two
/// `Option`s that can disagree, would leak the map's key type into both
/// clients, and would force each of them to re-derive the locking discipline —
/// which must be a **single `write()` held across the whole read-modify-write**,
/// never a read followed by a write, or two concurrent calls can lose a
/// failure. Binding the id once, at the four construction sites that know it,
/// puts that discipline in exactly one place. `Debug` and `Clone` are
/// load-bearing on the handle for the same reason they are on this type:
/// `ZapriteClient` derives both.
///
/// One thing the wiring must get right and cannot see from here: **record the
/// outcome as soon as the status is known, before reading the response body.**
/// Several call sites read the body with `?` ahead of the status check, so a
/// 401 whose body cannot be read produces no `ProviderHttpError` at all. That
/// is a pre-existing edge case today; recording after the body read would widen
/// it into the common path — a revoked key that happens to answer with a
/// truncated error body would then be invisible to this rule entirely. That is
/// the whole reason a 2xx that fails to parse still records a success: the
/// recording point is the status, not the outcome of the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAuthHealth {
    /// The key is not authenticating: a run of 401s, on any label. Cleared by
    /// any success anywhere, because a single 2xx proves the credential is
    /// still accepted.
    ///
    /// Reads to an operator as "authentication failing (key may be revoked)".
    /// Never as a flat "key revoked" — a 401 is the confirmed revocation
    /// signal on both providers, but it is also what a mistyped or
    /// half-rotated key looks like.
    pub auth_dead: FailureStreak,
    /// The key authenticates but cannot take money: a run of 403s on a
    /// hot-path label. Cleared **only** by a success on a
    /// [sell label](is_sell_label) — a call that actually created an invoice or
    /// charged a profile. Nothing else proves what this streak measures.
    ///
    /// Reads to an operator as "cannot create invoices (insufficient
    /// permissions)".
    ///
    /// **Note the asymmetry between what counts and what clears, because it is
    /// deliberate.** *Any* non-probe 403 counts, but only a *sell* success
    /// clears. Both lean the same way: toward keeping the signal rather than
    /// losing it.
    ///
    /// **The cost of that is on the counting side, and it is six labels, not
    /// one.** Only three of the nine labels the clients emit are
    /// [sell labels](is_sell_label); a 403 on any of the other six raises this
    /// streak without proving anything about `cancreateinvoice`:
    /// - `btcpay.get_invoice`, `zaprite.get_order` — need `canviewinvoices`.
    ///   **The highest-volume source by far**, because `reconcile.rs` calls
    ///   them every 60 seconds against every invoice pending under 72 hours.
    /// - `zaprite.get_contact`, `zaprite.create_contact` — contact-record
    ///   permissions, no money either way.
    /// - `zaprite.ping` — the operator's Connect validation.
    /// - `btcpay.pay_lightning_invoice` — needs `canuselightningnode` and pays
    ///   a tip **out**, so its 403 means "tips cannot pay out".
    ///
    /// The concrete case worth knowing before writing any operator-facing copy:
    /// a BTCPay key that **holds `cancreateinvoice` but is missing
    /// `canviewinvoices`** sells perfectly, yet with any invoice pending,
    /// reconcile 403s every 60 seconds — so this streak re-matures **eleven
    /// minutes** after each sale clears it, reading "cannot create invoices"
    /// over a healthy store. Note which way that cuts: it afflicts the **quiet**
    /// store, because a sale more often than every eleven minutes keeps
    /// clearing the streak before it can mature.
    /// `the_reconcile_403_red_returns_after_each_sale` pins both halves.
    ///
    /// Not introduced by the counter split — the counting side is unchanged
    /// since Step 2a, and it is inside the ratified direction (a visible false
    /// alarm is the safe side of this trade). But **Step 4a's wording must not
    /// claim this streak proves a checkout is broken**, and the Step 6 docs fix
    /// tracked in the plan's §10 covers all six labels, not just the tipping
    /// one it was originally raised for.
    pub cannot_sell: FailureStreak,
    /// The last time any call on this provider succeeded. `None` means no call
    /// has succeeded since this process started, which is not by itself a
    /// failure signal — it is also what "nothing has been sold yet" looks like.
    pub last_success_at: Option<SystemTime>,
    /// The most recent non-success HTTP status observed **since the last
    /// success**. Cleared by a success, so it never reads as `401` next to a
    /// healthy verdict.
    ///
    /// Note it is cleared by **any** success, including a probe's — so once the
    /// counters split, `last_status: None` can sit beside a running, even
    /// alerting, [`cannot_sell`](Self::cannot_sell). That is not a
    /// contradiction (the last thing observed really was a 200), but a
    /// consumer must report the *streak*, not this field, when naming why a
    /// provider is unhealthy.
    pub last_status: Option<u16>,
    /// When the provider first started returning 403 on a probe label. Never
    /// contributes to alerting and never disturbs either streak; it is
    /// reported as a non-alerting permissions observation, because on BTCPay a
    /// 403 is scope, not revocation. Holds the *first* such timestamp, not the
    /// latest, so it reads as "since when".
    ///
    /// Only a **success on a probe label** clears it, since that is the only
    /// evidence the missing permission was granted. It therefore persists if
    /// the probe starts failing some other way, or stops running at all —
    /// correctly, because in neither case has anything shown the permission
    /// came back.
    pub probe_403_since: Option<SystemTime>,
}

impl ProviderAuthHealth {
    /// Record a call that came back with a success status.
    ///
    /// `label` is load-bearing, because **what a success clears depends on what
    /// it proves**. Three independent questions, asked of every success:
    /// authentication (always answered), the probe's broader permission
    /// ([`is_probe_label`]), and invoice creation ([`is_sell_label`]). A label
    /// can answer one, two, or none of them.
    pub fn record_success(&mut self, label: &str, now: SystemTime) {
        self.last_success_at = Some(now);
        self.last_status = None;

        // Any success, on any label, proves the credential still authenticates.
        self.auth_dead.reset();

        // Deliberately two independent `if`s, not `if/else`. A probe label and
        // a sell label answer different questions, and most labels
        // (`get_invoice`, `get_contact`, `ping`, `pay_lightning_invoice`) are
        // neither, so they clear nothing beyond `auth_dead` above.
        if is_probe_label(label) {
            // The only evidence the broader permission came back.
            self.probe_403_since = None;
        }
        if is_sell_label(label) {
            // An invoice was actually created, so the operator can sell.
            self.cannot_sell.reset();
        }
    }

    /// Record a call that came back with a non-success HTTP status.
    ///
    /// A 401 extends [`auth_dead`](Self::auth_dead) on any label; a 403 extends
    /// [`cannot_sell`](Self::cannot_sell) only on a non-probe label, and on a
    /// probe label stamps [`probe_403_since`](Self::probe_403_since) instead.
    /// See the rule on the type. Every other status updates
    /// [`last_status`](Self::last_status) and nothing more.
    ///
    /// Callers must not pass a 2xx here — a success is
    /// [`record_success`](Self::record_success). Outcomes with no HTTP status
    /// at all (timeout, DNS/TLS failure, unreadable body, unparseable JSON)
    /// record **nothing**: there is no third entry point, because the correct
    /// behavior for them is to leave this struct exactly as it was.
    pub fn record_failure(&mut self, label: &str, status: u16, now: SystemTime) {
        // Structurally unreachable today: a client's `send()` returns `Ok` on
        // every success status, so nothing can route a 2xx here. The guard is
        // for the wiring, where the harm direction is worse than a misleading
        // `last_status` — a 2xx arriving here would fail to **reset** a running
        // streak, so one misrouted call turns into a persistent false alert
        // that no later success clears.
        debug_assert!(
            !(200..300).contains(&status),
            "record_failure got success status {status}; a 2xx is record_success"
        );

        self.last_status = Some(status);

        match status {
            // The confirmed revocation signal on both providers, and it is
            // label-blind: the probe exists precisely to find a dead key
            // before a buyer does.
            401 => self.auth_dead.count_failure(now),
            403 if is_probe_label(label) => {
                // A permission the sell path does not need. Stamp when it
                // started and leave **both** streaks alone — non-counting, and
                // just as importantly non-resetting.
                self.probe_403_since.get_or_insert(now);
            }
            // Whatever the cause of a 403 on a real call, that call cannot
            // take money.
            403 => self.cannot_sell.count_failure(now),
            _ => {}
        }
    }

    /// Is either condition bad enough to alert on?
    ///
    /// A roll-up for an overall `ok`/`critical` status only. A consumer that
    /// names the condition to an operator must read
    /// [`auth_dead`](Self::auth_dead) and [`cannot_sell`](Self::cannot_sell)
    /// separately — "authentication failing" and "cannot create invoices" are
    /// different problems with different remedies, and this method cannot tell
    /// them apart.
    pub fn is_alerting(&self, now: SystemTime) -> bool {
        self.auth_dead.is_alerting(now) || self.cannot_sell.is_alerting(now)
    }
}

/// The process-wide auth-health map, keyed by `payment_providers.id`.
///
/// Lives on `AppState`, is written by the provider clients through a
/// [`ProviderHealthSink`], and is read by the health-summary endpoint.
///
/// **A `std::sync::RwLock`, not `tokio`'s**, unlike `AppState`'s other locked
/// fields. The critical section is a pure in-memory read-modify-write with no
/// `await` in it, so a sync lock is both cheaper and *structurally* safer here:
/// holding a `std` guard across an `.await` does not compile, which is exactly
/// the mistake this map invites. Poisoning is recovered from rather than
/// propagated — a panic mid-update leaves the map internally consistent (every
/// mutation is a single infallible field write), and losing the whole alert
/// surface because one unrelated task panicked would be the worse failure.
///
/// In-memory only, so it empties on restart. Deliberate ceiling, documented on
/// [`ProviderAuthHealth`]; the upgrade path is a `provider_health` table.
///
/// **Keys are not garbage-collected.** Disconnecting a provider deletes its
/// row but leaves its entry here, and nothing removes it today. Pruning
/// orphans on read is the contract the health-summary endpoint is expected to
/// honor when it lands — stated here as the intended design, not as current
/// behavior, so a reader written before then does not assume every key still
/// names a live row.
pub type ProviderHealthMap = Arc<RwLock<HashMap<String, ProviderAuthHealth>>>;

/// A write handle onto [`ProviderHealthMap`], pre-bound to one provider row.
///
/// One of these is attached to a provider client at construction, at each of
/// the four sites that know which `payment_providers` row the client speaks
/// for. Everything downstream of that — the trait impls, the `as_any()`
/// downcast escapes in `subscriptions.rs`, the legacy `state.payment`
/// singleton — reaches the same client and therefore the same sink, which is
/// why tracking lives in the client rather than in a trait decorator.
///
/// The handle is `Option`al on the client, so a client built with no row to
/// key on records nothing at all: the Zaprite connect-time smoke test, and the
/// test fixtures that are exercising something other than this.
#[derive(Debug, Clone)]
pub struct ProviderHealthSink {
    map: ProviderHealthMap,
    /// `payment_providers.id`. Bound once, here, so neither client has to
    /// carry the map's key type.
    provider_id: String,
}

impl ProviderHealthSink {
    pub fn new(map: ProviderHealthMap, provider_id: impl Into<String>) -> Self {
        Self {
            map,
            provider_id: provider_id.into(),
        }
    }

    /// Fold one observed provider call into this provider's health.
    ///
    /// **The single entry point on purpose.** Routing on the status here, once,
    /// rather than at each client, is what makes it impossible for a wiring
    /// site to hand a 2xx to [`ProviderAuthHealth::record_failure`] — the slip
    /// that type's `debug_assert!` guards against, whose harm is a streak that
    /// never resets and so a false alert no later success can clear.
    ///
    /// `status` is the HTTP status the provider returned, and the caller must
    /// pass it **as soon as it is known, before reading the response body**;
    /// see the note on [`ProviderAuthHealth`]. An outcome that never reached a
    /// status — a timeout, a DNS or TLS failure — is not recorded at all, which
    /// is why there is no entry point for one.
    ///
    /// Success is `200..300`, matching `reqwest::StatusCode::is_success`, which
    /// is what both clients branch on to decide whether the call failed.
    pub fn record(&self, label: &str, status: u16, now: SystemTime) {
        // ONE `write()` held across the whole read-modify-write. A read
        // followed by a write would lose increments under exactly the
        // concurrency a shared provider client invites.
        let mut map = match self.map.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = map.entry(self.provider_id.clone()).or_default();
        if (200..300).contains(&status) {
            entry.record_success(label, now);
        } else {
            entry.record_failure(label, status, now);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The alert rule, driven entirely by injected timestamps.
    //!
    //! `tokio`'s `full` feature does not include `test-util`, so
    //! `tokio::time::pause` is unavailable and no test here may need it. That
    //! is why the clock is a parameter rather than a `SystemTime::now()` call
    //! inside the type.

    use super::*;

    /// A fixed point on the clock. `SystemTime::UNIX_EPOCH` plus `secs`, so
    /// every timestamp in these tests is a plain readable number.
    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A hot-path label — so its 403s count — that is *also* a sell label, so
    /// its successes clear `cannot_sell`. Tests that care about the difference
    /// between those two properties spell the label out instead.
    const HOT: &str = "zaprite.create_order";

    #[test]
    fn two_failures_do_not_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(3_600));

        assert_eq!(h.auth_dead.consecutive, 2);
        // An hour of failing is not enough on its own: the count arm holds.
        assert!(!h.is_alerting(t(3_600)));
    }

    #[test]
    fn three_failures_past_the_window_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(300));
        h.record_failure(HOT, 401, t(601));

        assert_eq!(h.auth_dead.consecutive, 3);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(0)));
        assert_eq!(h.last_status, Some(401));
        assert!(h.is_alerting(t(601)));
    }

    #[test]
    fn three_failures_inside_the_window_do_not_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(1));
        h.record_failure(HOT, 401, t(2));

        // Count arm met, wall-clock arm not: this is the shape of a key
        // rotation, three retries inside seconds.
        assert_eq!(h.auth_dead.consecutive, 3);
        assert!(!h.is_alerting(t(2)));
        assert!(!h.is_alerting(t(599)));
        // ...and it does become an alert once the streak is genuinely old,
        // with no further failures needed.
        assert!(h.is_alerting(t(600)));
    }

    #[test]
    fn first_failure_at_marks_the_start_of_the_streak_not_the_latest_failure() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(10));
        h.record_failure(HOT, 401, t(20));
        h.record_failure(HOT, 401, t(30));

        assert_eq!(h.auth_dead.first_failure_at, Some(t(10)));
    }

    #[test]
    fn a_success_resets_the_streak() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(100));
        h.record_failure(HOT, 401, t(700));
        assert!(h.is_alerting(t(700)));

        h.record_success(HOT, t(800));

        assert_eq!(h.auth_dead.consecutive, 0);
        assert_eq!(h.auth_dead.first_failure_at, None);
        assert_eq!(h.last_success_at, Some(t(800)));
        assert_eq!(h.last_status, None);
        // The alert clears on the recovering call itself, not on some later
        // sweep — this is the transition the admin card renders.
        assert!(!h.is_alerting(t(800)));
        assert!(!h.is_alerting(t(100_000)));

        // Two more failures after the reset are two, not five, so the streak
        // that starts at t(900) is what the wall clock is measured against.
        h.record_failure(HOT, 401, t(900));
        h.record_failure(HOT, 401, t(1_000));
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(900)));
        assert!(!h.is_alerting(t(100_000)));
    }

    #[test]
    fn transport_and_server_errors_are_inert_in_both_directions() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));
        // **Both** streaks must be running before the inert statuses arrive.
        // Against an empty streak, "does not count" and "resets" are
        // indistinguishable — the vacuity that nearly hid the hardest bug in
        // Step 2a, and the reason this test drives two streaks rather than one.
        h.record_failure(HOT, 403, t(70));
        h.record_failure(HOT, 403, t(80));
        assert_eq!(h.cannot_sell.consecutive, 2);

        // A 5xx: neither counts toward either streak...
        h.record_failure(HOT, 503, t(120));
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(0)));
        assert_eq!(h.cannot_sell.consecutive, 2);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(70)));
        // ...nor resets either. It is only an observation.
        assert_eq!(h.last_status, Some(503));

        // Same for a 429, and for the statuses that are neither auth nor
        // server errors.
        h.record_failure(HOT, 429, t(180));
        h.record_failure(HOT, 404, t(240));
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(0)));
        assert_eq!(h.cannot_sell.consecutive, 2);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(70)));

        // A timeout, or a DNS/TLS failure, never reaches a status, so nothing
        // is recorded for it and there is deliberately no entry point to call
        // here. That leg is inert *by construction* rather than by assertion,
        // and no test can pin it; what this test does pin is the half that
        // could regress, a real status arriving and disturbing a streak.

        // The third genuine failure still tips each one over, and it is the
        // third — the inert outcomes did not inflate either count.
        h.record_failure(HOT, 401, t(700));
        assert_eq!(h.auth_dead.consecutive, 3);
        assert!(h.auth_dead.is_alerting(t(700)));
        h.record_failure(HOT, 403, t(710));
        assert_eq!(h.cannot_sell.consecutive, 3);
        assert!(h.cannot_sell.is_alerting(t(710)));
    }

    #[test]
    fn a_hot_path_403_counts() {
        // One tracker is one `payment_providers` row, so every label in a
        // single tracker comes from the same provider.
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 403, t(0));
        h.record_failure("zaprite.get_order", 403, t(300));
        h.record_failure("zaprite.charge_order_with_profile", 403, t(900));

        assert_eq!(h.cannot_sell.consecutive, 3);
        // Whatever the cause of a 403 on the sell path, the operator cannot
        // sell, so it alerts.
        assert!(h.is_alerting(t(900)));
        assert_eq!(h.probe_403_since, None);
        // A 403 is a permissions statement, not an authentication one, so it
        // does not count toward `auth_dead`. Note this says only that: from an
        // empty tracker it cannot also show a 403 does not *reset* a running
        // `auth_dead`. `the_two_streaks_do_not_borrow_each_others_failures`
        // and `transport_and_server_errors_are_inert_in_both_directions` pin
        // that half, against streaks that are actually running.
        assert_eq!(h.auth_dead, FailureStreak::default());
        assert!(!h.auth_dead.is_alerting(t(900)));
    }

    #[test]
    fn a_probe_403_does_not_count_and_is_recorded() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(0));
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(1_000));
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(2_000));

        // The probe touches a broader permission than the sell path, so a key
        // that sells fine can 403 here forever.
        assert_eq!(h.cannot_sell, FailureStreak::default());
        assert_eq!(h.auth_dead, FailureStreak::default());
        assert!(!h.is_alerting(t(100_000)));

        // It is still reported, as a non-alerting observation, stamped from
        // when it started rather than when it was last seen.
        assert_eq!(h.probe_403_since, Some(t(0)));
        assert_eq!(h.last_status, Some(403));
    }

    /// The probe-403 rule's dangerous half: it must be **non-counting**, not
    /// **resetting**. Starting from an empty tracker cannot tell those two
    /// apart, and the difference is severe. BTCPay 403ing the probe forever is
    /// the documented normal case, the probe runs every 15 minutes and the
    /// alert window is 10 — so a probe 403 that quietly cleared a streak
    /// would wipe it before it could mature, and a genuinely revoked key would
    /// alert *never* rather than late. Same failure shape as the withdrawn
    /// `ever_succeeded` flag.
    ///
    /// Both streaks are running here, because a probe 403 must disturb
    /// **neither**.
    #[test]
    fn a_probe_403_does_not_disturb_either_running_streak() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));
        h.record_failure(HOT, 403, t(70));
        h.record_failure(HOT, 403, t(80));
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.cannot_sell.consecutive, 2);

        h.record_failure(ZAPRITE_PROBE_LABEL, 403, t(120));

        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(0)));
        assert_eq!(h.cannot_sell.consecutive, 2);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(70)));
        assert_eq!(h.probe_403_since, Some(t(120)));

        // Both were merely paused, and each still matures on its own count.
        h.record_failure(HOT, 401, t(180));
        h.record_failure(HOT, 403, t(190));
        assert_eq!(h.auth_dead.consecutive, 3);
        assert_eq!(h.cannot_sell.consecutive, 3);
        assert!(h.auth_dead.is_alerting(t(700)));
        assert!(h.cannot_sell.is_alerting(t(700)));
    }

    #[test]
    fn a_probe_401_counts_toward_auth_dead() {
        let mut h = ProviderAuthHealth::default();
        // A running sell-path streak first, so the assertions below can tell
        // "left alone" from "reset" — an empty `cannot_sell` cannot.
        h.record_failure(HOT, 403, t(0));
        h.record_failure(HOT, 403, t(10));

        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(20));
        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(400));
        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(800));

        // The probe-label exclusion is about 403 only. 401 is the confirmed
        // revocation signal on both providers and counts everywhere — the
        // probe exists precisely to find a revoked key before a buyer does.
        assert_eq!(h.auth_dead.consecutive, 3);
        assert_eq!(h.auth_dead.first_failure_at, Some(t(20)));
        assert!(h.auth_dead.is_alerting(t(800)));
        assert!(h.is_alerting(t(800)));
        // It is an authentication failure, not a permissions one: it must
        // neither extend the sell-path streak nor clear it, and the
        // permissions observation must stay clean — a spurious
        // `probe_403_since` here would put a "permissions" note beside a
        // genuine revoked-key alert.
        assert_eq!(h.cannot_sell.consecutive, 2);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(0)));
        assert_eq!(h.probe_403_since, None);
    }

    #[test]
    fn a_probe_success_clears_the_permissions_observation() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(0));
        assert_eq!(h.probe_403_since, Some(t(0)));

        // A success on the sell path says nothing about the broader permission
        // the probe needs, so the observation stands.
        h.record_success(HOT, t(100));
        assert_eq!(h.probe_403_since, Some(t(0)));

        // A success on the probe label is the evidence that it was granted.
        h.record_success(BTCPAY_PROBE_LABEL, t(200));
        assert_eq!(h.probe_403_since, None);
    }

    // -----------------------------------------------------------------
    // The split itself. Which success clears which streak is the whole
    // reason these are two counters and not one.
    // -----------------------------------------------------------------

    /// **The reason the counters are split.** Formerly
    /// `a_probe_success_currently_resets_a_running_streak`, which pinned the
    /// old single-counter behavior specifically so this change would fail
    /// loudly rather than slide through.
    ///
    /// A probe success proves the credential authenticates, so it clears
    /// `auth_dead`. It proves **nothing** about `cancreateinvoice`, so it must
    /// leave `cannot_sell` exactly where it was. Letting it clear both is the
    /// scope-broken-key hole: with a 15-minute probe cadence against a
    /// 10-minute window, a key that authenticates but cannot sell would alert
    /// only if three checkouts 403 inside the ten minutes after a probe
    /// success — on a low-traffic instance, possibly never.
    #[test]
    fn a_probe_success_clears_auth_dead_but_not_cannot_sell() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));
        h.record_failure(HOT, 403, t(70));
        h.record_failure(HOT, 403, t(80));
        h.record_failure(HOT, 403, t(90));
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.cannot_sell.consecutive, 3);

        h.record_success(ZAPRITE_PROBE_LABEL, t(120));

        // Authentication is demonstrably fine now.
        assert_eq!(h.auth_dead.consecutive, 0);
        assert_eq!(h.auth_dead.first_failure_at, None);
        assert!(!h.auth_dead.is_alerting(t(100_000)));

        // Selling is not, and the probe had nothing to say about it. The
        // start timestamp survives too, or the wall-clock arm would restart
        // and the alert would be pushed another ten minutes out on every
        // probe.
        assert_eq!(h.cannot_sell.consecutive, 3);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(70)));
        assert!(h.cannot_sell.is_alerting(t(700)));
        assert!(h.is_alerting(t(700)));

        assert_eq!(h.last_success_at, Some(t(120)));
    }

    /// The other half: a sell-label success is a real invoice going through,
    /// so it clears both. This is what makes a fixed key go green on the next
    /// sale rather than lingering red forever.
    #[test]
    fn a_sell_success_clears_both_streaks() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));
        h.record_failure(HOT, 401, t(700));
        h.record_failure(HOT, 403, t(710));
        h.record_failure(HOT, 403, t(720));
        h.record_failure(HOT, 403, t(1_400));
        assert!(h.auth_dead.is_alerting(t(1_400)));
        assert!(h.cannot_sell.is_alerting(t(1_400)));

        h.record_success("btcpay.create_invoice", t(1_500));

        assert_eq!(h.auth_dead, FailureStreak::default());
        assert_eq!(h.cannot_sell, FailureStreak::default());
        assert!(!h.is_alerting(t(100_000)));
        assert_eq!(h.last_success_at, Some(t(1_500)));
        assert_eq!(h.last_status, None);
    }

    /// **The reconcile hole, closed.** `reconcile.rs` ticks every 60 seconds
    /// and calls `get_invoice_status` on every invoice pending under 72 hours,
    /// emitting `btcpay.get_invoice` / `zaprite.get_order`. Those need
    /// `canviewinvoices`, not `cancreateinvoice`. If a non-sell success
    /// cleared this streak, then on any instance with a pending invoice a key
    /// that had lost `cancreateinvoice` would collect a 200 every minute —
    /// about ten resets per ten-minute alert window — and `cannot_sell`, which
    /// needs three *consecutive* 403s spanning that window, could never mature.
    /// Same never-alerts outcome the split was chosen to prevent, at a 15×
    /// tighter cadence than the probe.
    ///
    /// Every non-sell success is driven here, not just reconcile's, because
    /// each one is a separate way to reopen it.
    #[test]
    fn a_non_selling_success_does_not_clear_cannot_sell() {
        for label in [
            // The reconcile sweep — the case that forced the allow-list.
            "btcpay.get_invoice",
            "zaprite.get_order",
            // Writes, but not money.
            "zaprite.create_contact",
            "zaprite.get_contact",
            // The operator's Connect validation.
            "zaprite.ping",
            // Pays a tip *out*; needs `canuselightningnode`.
            "btcpay.pay_lightning_invoice",
            // The liveness probes.
            BTCPAY_PROBE_LABEL,
            ZAPRITE_PROBE_LABEL,
            // A label nobody has classified. The allow-list means an unknown
            // call cannot silently earn the right to clear this.
            "btcpay.something_added_later",
        ] {
            let mut h = ProviderAuthHealth::default();
            // A **running** `auth_dead` as well, so the assertion below that
            // the success cleared it can tell "reset" from "never set". With
            // only 403s seeded, `auth_dead` would be default either way and
            // the assertion would hold even if `record_success` never touched
            // it — the vacuity this file has to keep watching for.
            h.record_failure("btcpay.create_invoice", 401, t(0));
            h.record_failure("btcpay.create_invoice", 401, t(10));
            h.record_failure("btcpay.create_invoice", 403, t(20));
            h.record_failure("btcpay.create_invoice", 403, t(300));
            h.record_failure("btcpay.create_invoice", 403, t(700));
            assert_eq!(h.auth_dead.consecutive, 2);
            assert!(h.cannot_sell.is_alerting(t(700)));

            h.record_success(label, t(760));

            assert_eq!(
                h.cannot_sell.consecutive, 3,
                "{label} does not prove an invoice can be created"
            );
            assert_eq!(h.cannot_sell.first_failure_at, Some(t(20)));
            assert!(
                h.cannot_sell.is_alerting(t(760)),
                "{label} must not silence a matured cannot_sell alert"
            );
            // ...while still proving the credential authenticates, which is
            // why `auth_dead` — genuinely running a moment ago — is now clear.
            assert_eq!(
                h.auth_dead,
                FailureStreak::default(),
                "{label} returned 2xx, so the key authenticates"
            );
            assert_eq!(h.last_success_at, Some(t(760)));
        }
    }

    /// **The counting side's cost, driven end to end so it is a known reality
    /// rather than a surprise.**
    ///
    /// A BTCPay key that holds `cancreateinvoice` but is missing
    /// `canviewinvoices` sells perfectly and reconciles not at all. Checkouts
    /// 200; `reconcile.rs`, ticking every 60 seconds against every invoice
    /// pending under 72 hours, 403s. Each sale clears `cannot_sell`, and
    /// **eleven minutes later it has matured again** — three reconcile 403s at
    /// 60-second spacing satisfy the count arm within three minutes, then the
    /// wall-clock arm ten minutes after the first of them.
    ///
    /// Which means the false red depends on sale cadence, in the direction
    /// nobody guesses: it afflicts the **quiet** store, not the busy one. A
    /// sale more often than every eleven minutes keeps clearing the streak
    /// before it can mature, and the operator never sees it. Both halves are
    /// asserted below, because reasoning about this one from the rule alone is
    /// what produced a wrong answer the first time.
    ///
    /// It is inside the ratified direction — a visible false alarm is the safe
    /// side — and it predates the counter split, since the *counting* side is
    /// unchanged since Step 2a. Pinned because it is exactly what an operator
    /// would report as a bug, and **Step 4a's wording has to be written knowing
    /// it happens.**
    #[test]
    fn the_reconcile_403_red_returns_after_each_sale() {
        /// Ninety minutes of a healthy checkout path and a broken reconcile
        /// path: a sale every `sale_every` minutes, a reconcile 403 in every
        /// minute between. Returns the minutes in which `cannot_sell` alerts.
        fn run(sale_every: u64) -> (Vec<u64>, ProviderAuthHealth) {
            let mut h = ProviderAuthHealth::default();
            let mut alerting = Vec::new();
            for minute in 0..90u64 {
                if minute % sale_every == 0 {
                    h.record_success("btcpay.create_invoice", t(minute * 60));
                } else {
                    h.record_failure("btcpay.get_invoice", 403, t(minute * 60));
                }
                if h.cannot_sell.is_alerting(t(minute * 60)) {
                    alerting.push(minute);
                }
            }
            (alerting, h)
        }

        // A sale every half hour. The red returns 11 minutes after each one
        // and stands until the next sale clears it — about two thirds of the
        // time, over a store whose checkout has never once failed.
        let (alerting, h) = run(30);
        let expected: Vec<u64> = (11..30).chain(41..60).chain(71..90).collect();
        assert_eq!(alerting, expected);

        // And it is genuinely the reconcile 403s driving it: the checkout path
        // never failed, so `auth_dead` never moved.
        assert_eq!(h.auth_dead, FailureStreak::default());
        assert_eq!(h.last_status, Some(403));

        // A sale every five minutes — a busier store — and the operator never
        // sees it at all. The wall-clock arm never completes.
        assert_eq!(run(5).0, Vec::<u64>::new());

        // The mirror, for contrast: had `canviewinvoices` been the healthy one
        // and `cancreateinvoice` the broken one, nothing would ever clear the
        // streak and the alert would simply stand. That is the case this
        // streak exists for, and it is unaffected by cadence.
        let mut h = ProviderAuthHealth::default();
        for minute in 0..90u64 {
            if minute % 30 == 0 {
                h.record_failure("btcpay.create_invoice", 403, t(minute * 60));
            } else {
                h.record_success("btcpay.get_invoice", t(minute * 60));
            }
        }
        assert!(h.cannot_sell.is_alerting(t(90 * 60)));
    }

    /// The inverse, and the reason the allow-list is not simply "never clear".
    /// A real sale going through is exactly the evidence `cannot_sell` wants,
    /// so each money call must clear it — otherwise a fixed key stays red
    /// forever and the signal becomes noise.
    #[test]
    fn a_sell_label_success_clears_cannot_sell() {
        for label in SELL_LABELS {
            let mut h = ProviderAuthHealth::default();
            h.record_failure("btcpay.create_invoice", 403, t(0));
            h.record_failure("btcpay.create_invoice", 403, t(300));
            h.record_failure("btcpay.create_invoice", 403, t(700));
            assert!(h.cannot_sell.is_alerting(t(700)));

            h.record_success(label, t(760));

            assert_eq!(
                h.cannot_sell,
                FailureStreak::default(),
                "{label} created an invoice"
            );
            assert!(!h.is_alerting(t(100_000)));
        }
    }

    /// `auth_dead` is deliberately **not** narrowed by the allow-list: any 2xx
    /// from the provider proves the credential was accepted, whatever the call
    /// was. Narrowing it to sell labels would mean a revoked key that got fixed
    /// stayed red until the next purchase, and would make the 15-minute probe —
    /// whose entire job is to answer this question — unable to answer it.
    #[test]
    fn auth_dead_is_cleared_by_every_success_including_non_sell_ones() {
        for label in [
            "btcpay.get_invoice",
            "zaprite.get_order",
            "zaprite.get_contact",
            "zaprite.create_contact",
            "zaprite.ping",
            "btcpay.pay_lightning_invoice",
            BTCPAY_PROBE_LABEL,
            ZAPRITE_PROBE_LABEL,
            "btcpay.something_added_later",
        ] {
            let mut h = ProviderAuthHealth::default();
            h.record_failure("btcpay.create_invoice", 401, t(0));
            h.record_failure("btcpay.create_invoice", 401, t(300));
            h.record_failure("btcpay.create_invoice", 401, t(700));
            assert!(h.auth_dead.is_alerting(t(700)));

            h.record_success(label, t(760));

            assert_eq!(
                h.auth_dead,
                FailureStreak::default(),
                "{label} returned 2xx, so the key authenticates"
            );
            assert!(!h.is_alerting(t(100_000)));
        }
    }

    /// **The membership pin.** Widening the set of successes that clear
    /// `cannot_sell` is precisely how this bug comes back, so the allow-list is
    /// asserted verbatim rather than by rule. Adding a label here should be a
    /// deliberate act with a justification, not a side effect of adding a call.
    ///
    /// A label must never be both a probe and a sell call: the probe exists to
    /// authenticate *without* creating an invoice, and a label that did both
    /// would make every probe run clear `cannot_sell`.
    ///
    /// Compared **sorted**, because membership is the invariant and reordering
    /// the array changes nothing about behavior.
    #[test]
    fn sell_labels_are_exactly_the_invoice_creating_calls() {
        let mut actual = SELL_LABELS.to_vec();
        actual.sort_unstable();
        let mut expected = [
            "btcpay.create_invoice",
            "zaprite.create_order",
            "zaprite.charge_order_with_profile",
        ];
        expected.sort_unstable();
        assert_eq!(actual, expected);

        // Every other label the clients emit today is not a sell label.
        for label in [
            "btcpay.pay_lightning_invoice",
            "btcpay.get_invoice",
            "zaprite.get_order",
            "zaprite.create_contact",
            "zaprite.get_contact",
            "zaprite.ping",
            BTCPAY_PROBE_LABEL,
            ZAPRITE_PROBE_LABEL,
            "btcpay.something_added_later",
        ] {
            assert!(!is_sell_label(label), "{label} must not be a sell label");
        }

        for label in SELL_LABELS {
            assert!(is_sell_label(label));
            assert!(
                !is_probe_label(label),
                "{label} cannot be both a probe and a sell call"
            );
        }
    }

    /// A 401 belongs to `auth_dead` alone and a hot-path 403 to `cannot_sell`
    /// alone. If either leaked into the other, three failures split two-and-one
    /// across the two causes would alert as though they were three of one — the
    /// merged counter this step removed.
    #[test]
    fn the_two_streaks_do_not_borrow_each_others_failures() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(10));
        h.record_failure(HOT, 403, t(20));

        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.cannot_sell.consecutive, 1);
        // Three counting failures in total, but neither streak reached three,
        // so nothing alerts however long it runs.
        assert!(!h.is_alerting(t(100_000)));
    }

    /// `auth_dead`'s two arms, pinned at both boundaries, with `cannot_sell`
    /// silent throughout.
    #[test]
    fn auth_dead_alerts_on_its_own_two_arms() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(BTCPAY_PROBE_LABEL, 401, t(300));
        // Count arm: two is not enough, no matter how old.
        assert_eq!(h.auth_dead.consecutive, 2);
        assert!(!h.auth_dead.is_alerting(t(100_000)));

        h.record_failure(HOT, 401, t(600));
        assert_eq!(h.auth_dead.consecutive, 3);
        // Wall-clock arm, pinned at the second either side of the boundary.
        assert!(!h.auth_dead.is_alerting(t(599)));
        assert!(h.auth_dead.is_alerting(t(600)));

        // The other condition never fired: a revoked key is not a permissions
        // problem, and Step 4a must not report it as one.
        assert!(!h.cannot_sell.is_alerting(t(100_000)));
        assert_eq!(h.cannot_sell, FailureStreak::default());
    }

    /// `cannot_sell`'s two arms, pinned at both boundaries, with `auth_dead`
    /// silent throughout — and with a probe success interleaved, which must
    /// change nothing here.
    #[test]
    fn cannot_sell_alerts_on_its_own_two_arms() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 403, t(0));
        h.record_failure("btcpay.create_invoice", 403, t(200));
        // The probe is happily returning 200 the whole time, which is exactly
        // the scope-broken-key case: authentication is fine, selling is not.
        h.record_success(BTCPAY_PROBE_LABEL, t(300));
        assert_eq!(h.cannot_sell.consecutive, 2);
        assert!(!h.cannot_sell.is_alerting(t(100_000)));

        h.record_failure(HOT, 403, t(600));
        assert_eq!(h.cannot_sell.consecutive, 3);
        assert_eq!(h.cannot_sell.first_failure_at, Some(t(0)));
        assert!(!h.cannot_sell.is_alerting(t(599)));
        assert!(h.cannot_sell.is_alerting(t(600)));
        assert!(h.is_alerting(t(600)));

        // Authentication was never in question.
        assert!(!h.auth_dead.is_alerting(t(100_000)));
        assert_eq!(h.auth_dead, FailureStreak::default());
    }

    /// The restart regression. StartOS restarts the daemon on every package
    /// update, and this map is in memory, so the very first call a fresh
    /// process observes can be the failing one.
    ///
    /// An earlier revision of the rule gated 403 handling on a runtime
    /// `ever_succeeded` flag. An already-revoked key can never flip such a flag
    /// true, so after any restart the alert could never fire again — a
    /// permanent miss, not a delayed one. This pins that the rule reads only
    /// the current call's label and status: a tracker with no history at all
    /// must still reach an alert, on **either** streak.
    #[test]
    fn fresh_tracker_after_restart_still_alerts() {
        let mut h = ProviderAuthHealth::default();
        assert_eq!(h, ProviderAuthHealth::default());
        assert_eq!(h.last_success_at, None);

        h.record_failure("btcpay.create_invoice", 401, t(0));
        h.record_failure("btcpay.create_invoice", 401, t(500));
        h.record_failure("btcpay.get_invoice", 401, t(1_000));

        assert!(h.auth_dead.is_alerting(t(1_000)));
        assert!(h.is_alerting(t(1_000)));

        // And the same holds for the hot-path 403 arm, the other half that the
        // withdrawn `ever_succeeded` flag would have suppressed — now its own
        // streak, so it has to reach the alert entirely on its own.
        let mut h = ProviderAuthHealth::default();
        assert_eq!(h, ProviderAuthHealth::default());
        h.record_failure("btcpay.create_invoice", 403, t(0));
        h.record_failure("btcpay.create_invoice", 403, t(500));
        h.record_failure("btcpay.create_invoice", 403, t(1_000));
        assert!(h.cannot_sell.is_alerting(t(1_000)));
        assert!(h.is_alerting(t(1_000)));

        // A cold start whose very first call is a probe 401 still reaches the
        // alert too: nothing about the rule requires a prior success.
        let mut h = ProviderAuthHealth::default();
        h.record_failure(BTCPAY_PROBE_LABEL, 401, t(0));
        h.record_failure(BTCPAY_PROBE_LABEL, 401, t(500));
        h.record_failure(BTCPAY_PROBE_LABEL, 401, t(1_000));
        assert!(h.auth_dead.is_alerting(t(1_000)));
        assert!(h.is_alerting(t(1_000)));
    }

    /// Every label the clients emit today, verbatim. None may classify as a
    /// probe — `zaprite.ping` least of all: it is the operator's Connect
    /// validation call, where a 403 is a genuine "your key cannot do this" and
    /// must keep counting. The liveness probe reuses that HTTP call but has to
    /// pass [`ZAPRITE_PROBE_LABEL`] instead, which is why the rule keys on the
    /// call site's label rather than the method.
    #[test]
    fn no_production_call_label_is_a_probe_label() {
        for label in [
            "btcpay.create_invoice",
            "btcpay.pay_lightning_invoice",
            "btcpay.get_invoice",
            "zaprite.create_order",
            "zaprite.get_order",
            "zaprite.charge_order_with_profile",
            "zaprite.create_contact",
            "zaprite.get_contact",
            "zaprite.ping",
        ] {
            assert!(!is_probe_label(label), "{label} must not be a probe label");
        }

        assert!(is_probe_label(BTCPAY_PROBE_LABEL));
        assert!(is_probe_label(ZAPRITE_PROBE_LABEL));
        // An unrecognized label is not a probe, so its 403s count.
        assert!(!is_probe_label("btcpay.something_added_later"));
    }

    /// A 2xx routed into `record_failure` is a wiring bug, not an input, and
    /// the guard exists because of where the harm lands. It is not the
    /// misleading `last_status`: a 2xx arriving here would fail to **reset** a
    /// running streak, so a single misrouted call in the wiring becomes a
    /// persistent false alert that no later success clears.
    ///
    /// Gated on `debug_assertions` because a release build compiles the
    /// assertion out; there the call degrades to the inert path — it records
    /// the status and leaves both counters alone.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "record_failure got success status 200")]
    fn a_success_status_routed_into_record_failure_is_caught() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 200, t(0));
    }

    // -----------------------------------------------------------------
    // The sink handle. The rule above is pure; these cover the shared map
    // it is folded into and the status routing that sits in front of it.
    // -----------------------------------------------------------------

    fn sink_for(map: &ProviderHealthMap, id: &str) -> ProviderHealthSink {
        ProviderHealthSink::new(map.clone(), id)
    }

    fn health_of(map: &ProviderHealthMap, id: &str) -> ProviderAuthHealth {
        map.read().expect("not poisoned").get(id).cloned().unwrap_or_default()
    }

    /// The reason `record` is the single entry point: it, not the caller,
    /// decides which of the two folds a status is. A wiring site cannot route
    /// a 2xx into `record_failure` — the slip whose `debug_assert!` above
    /// exists because it would leave a streak that no later success clears.
    #[test]
    fn the_sink_routes_a_2xx_to_success_and_everything_else_to_failure() {
        let map: ProviderHealthMap = Default::default();
        let sink = sink_for(&map, "prov-1");

        sink.record(HOT, 401, t(0));
        sink.record(HOT, 401, t(60));
        assert_eq!(health_of(&map, "prov-1").auth_dead.consecutive, 2);

        // A 2xx folds as a success — no panic, and the streak resets.
        sink.record(HOT, 200, t(120));
        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 0);
        assert_eq!(h.last_success_at, Some(t(120)));
        assert_eq!(h.last_status, None);

        // The boundaries of the success range, which must match
        // `reqwest::StatusCode::is_success` — the same test both clients apply
        // when deciding whether the call failed.
        sink.record(HOT, 299, t(130));
        assert_eq!(health_of(&map, "prov-1").last_success_at, Some(t(130)));
        sink.record(HOT, 300, t(140));
        assert_eq!(health_of(&map, "prov-1").last_status, Some(300));
        sink.record(HOT, 199, t(150));
        assert_eq!(health_of(&map, "prov-1").last_status, Some(199));
    }

    /// The sink routes on status alone, so the label-dependent half of the
    /// rule has to survive the trip through it: a probe success arriving via
    /// `record` must still leave `cannot_sell` alone.
    #[test]
    fn the_sink_preserves_the_label_dependent_success_rule() {
        let map: ProviderHealthMap = Default::default();
        let sink = sink_for(&map, "prov-1");

        sink.record(HOT, 401, t(0));
        sink.record(HOT, 403, t(10));
        sink.record(HOT, 403, t(20));
        sink.record(HOT, 403, t(700));
        assert!(health_of(&map, "prov-1").cannot_sell.is_alerting(t(700)));

        sink.record(BTCPAY_PROBE_LABEL, 200, t(800));

        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 0);
        assert_eq!(h.cannot_sell.consecutive, 3);
        assert!(h.cannot_sell.is_alerting(t(800)));

        // A hot-path success then clears it.
        sink.record(HOT, 200, t(900));
        assert_eq!(health_of(&map, "prov-1").cannot_sell.consecutive, 0);
    }

    /// One entry per `payment_providers` row. A shared map with a mis-bound key
    /// would blend two providers' streaks and alert on the wrong one.
    #[test]
    fn sinks_on_one_map_are_keyed_by_provider_id() {
        let map: ProviderHealthMap = Default::default();
        let a = sink_for(&map, "prov-a");
        let b = sink_for(&map, "prov-b");

        a.record(HOT, 401, t(0));
        a.record(HOT, 401, t(300));
        a.record(HOT, 401, t(700));
        b.record(HOT, 200, t(700));

        assert!(health_of(&map, "prov-a").is_alerting(t(700)));
        assert!(!health_of(&map, "prov-b").is_alerting(t(700)));
        assert_eq!(map.read().expect("not poisoned").len(), 2);
        // A provider nothing has been observed for has no entry at all, which
        // is how a consumer tells "never called" from "called and healthy".
        assert!(!map.read().expect("not poisoned").contains_key("prov-c"));
    }

    /// **Map membership means "a status was observed", not "something was
    /// learned about the key".** An inert status — a 5xx, a 429 — creates an
    /// entry that reads as healthy with a non-2xx `last_status`, which is the
    /// honest reading: the provider answered, and what it answered says nothing
    /// about authentication either way.
    ///
    /// Pinned because a consumer will want to tell "never called" from "called
    /// and fine", and this is the line it has to draw on. Reporting a provider
    /// whose only traffic was a 502 as never-called would be false; reporting
    /// its auth as failing would be worse.
    #[test]
    fn an_inert_status_still_creates_an_entry() {
        let map: ProviderHealthMap = Default::default();
        let sink = sink_for(&map, "prov-1");

        assert!(!map.read().expect("not poisoned").contains_key("prov-1"));

        sink.record(HOT, 502, t(0));

        assert!(
            map.read().expect("not poisoned").contains_key("prov-1"),
            "a provider that answered 502 has been called"
        );
        let h = health_of(&map, "prov-1");
        assert_eq!(h.last_status, Some(502));
        assert_eq!(h.auth_dead, FailureStreak::default());
        assert_eq!(h.cannot_sell, FailureStreak::default());
        assert_eq!(h.last_success_at, None);
        assert!(!h.is_alerting(t(100_000)));
    }

    /// The read-modify-write must be one `write()`, never a read followed by a
    /// write. Under a shared client — which is what an `Arc<dyn PaymentProvider>`
    /// handed to concurrent requests is — the read-then-write shape drops
    /// increments, and a dropped increment is a missed alert.
    #[test]
    fn concurrent_records_do_not_lose_increments() {
        use std::thread;

        let map: ProviderHealthMap = Default::default();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let sink = sink_for(&map, "prov-1");
                thread::spawn(move || {
                    for i in 0..100 {
                        sink.record(HOT, 401, t(i));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("worker thread");
        }

        assert_eq!(health_of(&map, "prov-1").auth_dead.consecutive, 800);
    }

    #[test]
    fn a_backwards_clock_does_not_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(10_000));
        h.record_failure(HOT, 401, t(10_001));
        h.record_failure(HOT, 401, t(10_002));

        // The streak started in what is now the future. A system clock
        // correction is not evidence of a revoked key.
        assert!(!h.is_alerting(t(0)));
        // Once the clock is past the window again, it alerts normally.
        assert!(h.is_alerting(t(10_600)));
    }
}
