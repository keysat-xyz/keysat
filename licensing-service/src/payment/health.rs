//! Per-provider API-authentication health — the decision layer behind the
//! operator failure alerts.
//!
//! Keysat can lose the ability to sell without anyone noticing: if the
//! operator's BTCPay or Zaprite API key is revoked, nothing surfaces it until
//! a buyer reaches checkout and cannot pay. [`ProviderAuthHealth`] is what
//! notices. It folds a stream of observed provider-call outcomes into the one
//! question an operator cares about — "is this provider's key still working?"
//!
//! Everything here is pure: no IO, no lock, no ambient clock (every entry
//! point takes `now`). The shared map, the sink handle and the client
//! attachment sites are wiring and land separately.

use std::time::{Duration, SystemTime};

/// Consecutive counting auth failures required before an alert can fire.
///
/// The count arm is what kills a fluke — one 401 from a provider having a bad
/// minute is not a revoked key.
pub const ALERT_MIN_CONSECUTIVE_FAILURES: u32 = 3;

/// How long the failure streak must already have been running before an alert
/// can fire.
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
/// Delegating to `ping` unchanged would file every probe 403 under
/// `"zaprite.ping"`, which is not a probe label, and the daemon would alert on
/// a key that sells perfectly well. The label's type is `&'static str`, so the
/// compiler will not catch it either — the probe has to thread this constant
/// through explicitly.
pub const ZAPRITE_PROBE_LABEL: &str = "zaprite.probe_auth";

/// The complete set of labels treated as liveness probes.
const PROBE_LABELS: &[&str] = &[BTCPAY_PROBE_LABEL, ZAPRITE_PROBE_LABEL];

/// Is this call label a liveness probe rather than a real piece of work?
///
/// Only 403 handling depends on the answer (a 401 counts everywhere), and the
/// distinction is real: the probe deliberately touches a broader permission
/// than the sell path — BTCPay's `canviewstoresettings` versus the
/// `cancreateinvoice` a checkout actually needs — so a key that sells perfectly
/// well can 403 on the probe forever.
///
/// **Membership is by call-site label, never by method name.** One HTTP call
/// can be two different things depending on who invoked it, and `ping` is
/// exactly that case: operator Connect validation today, liveness probe once
/// the probe lands. Keying on the method would silently stop counting a genuine
/// Connect-time 403.
///
/// Unknown labels are classified as **not** a probe, so a call added later
/// without a thought for this rule has its 403s counted. That direction fails
/// toward a visible, correctable false alarm rather than toward silently losing
/// the signal — and a lone misclassified 403 still cannot fire anything on its
/// own, because alerting needs three of them spread over ten minutes.
pub fn is_probe_label(label: &str) -> bool {
    PROBE_LABELS.contains(&label)
}

/// One payment provider's authentication health, folded from observed calls.
///
/// # The rule
///
/// - **401 always counts** as an auth failure, on every label including the
///   probe. Verified 2026-07-24 against both providers: a nonexistent key
///   returns 401, and revocation deletes the token record server-side so it
///   takes the same path.
/// - **403 counts on any non-probe label.** Whatever the underlying cause, the
///   operator cannot sell.
/// - **403 on a probe label never counts.** It is recorded as
///   [`probe_403_since`](Self::probe_403_since) and reported as a non-alerting
///   permissions observation.
/// - A counting failure increments [`consecutive_auth_failures`](Self::consecutive_auth_failures)
///   and stamps [`first_failure_at`](Self::first_failure_at) **on the 0→1
///   transition only**, so the timestamp marks when the streak began rather
///   than when it was last extended.
/// - Any success resets the counter, clears `first_failure_at`, and sets
///   [`last_success_at`](Self::last_success_at).
/// - Every other **status** — 5xx, 429, 404 — is **inert**: it neither
///   increments nor resets. A provider outage must not manufacture an auth
///   alert, and must not paper over one either. So is an outcome that never
///   reached a status at all (a timeout, a DNS or TLS failure), because
///   nothing is recorded for it.
///
///   Note what that does *not* say. Once a status is known the call is
///   recorded, so a 2xx whose body is unreadable or whose JSON does not parse
///   still records a **success**, and a 401 whose body is unreadable still
///   **counts**. Both are deliberate; see the wiring note at the bottom.
/// - **Alerting** = `consecutive_auth_failures >= `[`ALERT_MIN_CONSECUTIVE_FAILURES`]
///   **and** the streak is at least [`ALERT_MIN_STREAK`] old.
///
/// # Invariant: no in-memory history may gate alerting
///
/// This map is in-memory, and StartOS restarts the daemon on every package
/// update. An earlier revision of this rule gated the 403 handling on a runtime
/// `ever_succeeded` flag; because an already-revoked key can never flip such a
/// flag true, the miss was **permanent** across every future restart. So there
/// is deliberately no field of that shape here, and there must never be one:
/// the rule reads only the current call's label and status. A tracker that has
/// just been created must be able to reach an alert from a cold start, which
/// `fresh_tracker_after_restart_still_alerts` pins.
///
/// The in-memory-ness is itself a documented ceiling — a restart forgets the
/// streak and the operator waits out another ten minutes — with the upgrade
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
/// **2. A client will hold one pre-bound sink handle, not `(sink, provider_id)`.**
/// When the clients are wired up, `BtcpayClient` and `ZapriteClient` each get a
/// single `Option<ProviderHealthSink>` field, shaped roughly:
///
/// ```text
/// #[derive(Debug, Clone)]
/// pub struct ProviderHealthSink {
///     map: Arc<RwLock<HashMap<String, ProviderAuthHealth>>>, // key: payment_providers.id
///     provider_id: String,
/// }
/// impl ProviderHealthSink {
///     pub fn record_success(&self, label: &str, now: SystemTime);
///     pub fn record_failure(&self, label: &str, status: u16, now: SystemTime);
/// }
/// ```
///
/// attached through a `with_sink()` builder rather than a `new()` parameter, so
/// the test-only `ZapriteClient::new` call sites keep compiling. Two fields
/// would be two `Option`s that can disagree, would leak the map's key type into
/// both clients, and would force each of them to re-derive the locking
/// discipline — which must be a **single `write()` held across the whole
/// read-modify-write**, never a read followed by a write, or two concurrent
/// calls can lose a failure. Binding the id once, at the four construction
/// sites that know it, puts that discipline in exactly one place. `Debug` and
/// `Clone` are load-bearing on the handle for the same reason they are on this
/// type: `ZapriteClient` derives both.
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
    /// Length of the current run of counting auth failures. Reset by any
    /// success; untouched by outcomes that are neither.
    pub consecutive_auth_failures: u32,
    /// When the current streak began — stamped on the 0→1 transition and left
    /// alone while the streak extends. `None` whenever the counter is 0.
    pub first_failure_at: Option<SystemTime>,
    /// The last time any call on this provider succeeded. `None` means no call
    /// has succeeded since this process started, which is not by itself a
    /// failure signal — it is also what "nothing has been sold yet" looks like.
    pub last_success_at: Option<SystemTime>,
    /// The most recent non-success HTTP status observed **since the last
    /// success**. Cleared by a success, so it never reads as `401` next to a
    /// healthy verdict.
    pub last_status: Option<u16>,
    /// When the provider first started returning 403 on a probe label. Never
    /// contributes to alerting; it is reported as a non-alerting permissions
    /// observation, because on BTCPay a 403 is scope, not revocation. Holds the
    /// *first* such timestamp, not the latest, so it reads as "since when".
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
    /// `label` is taken for the same reason [`record_failure`](Self::record_failure)
    /// takes it: a success on a *probe* label is the only evidence that a
    /// previously missing permission has been granted, so it is what clears
    /// [`probe_403_since`](Self::probe_403_since). Without it the permissions
    /// observation would stick until the daemon restarted, long after the
    /// operator fixed the key.
    pub fn record_success(&mut self, label: &str, now: SystemTime) {
        self.consecutive_auth_failures = 0;
        self.first_failure_at = None;
        self.last_success_at = Some(now);
        self.last_status = None;
        if is_probe_label(label) {
            self.probe_403_since = None;
        }
    }

    /// Record a call that came back with a non-success HTTP status.
    ///
    /// Only 401 and a non-probe 403 count; see the rule on the type. Anything
    /// else updates [`last_status`](Self::last_status) and nothing more.
    ///
    /// Callers must not pass a 2xx here — a success is
    /// [`record_success`](Self::record_success). Outcomes with no HTTP status
    /// at all (timeout, DNS/TLS failure, unreadable body, unparseable JSON)
    /// record **nothing**: there is no third entry point, because the correct
    /// behavior for them is to leave this struct exactly as it was.
    pub fn record_failure(&mut self, label: &str, status: u16, now: SystemTime) {
        // Structurally unreachable today: a client's `send()` returns `Ok` on
        // every success status, so nothing can route a 2xx here. The guard is
        // for the wiring that comes next, where the harm direction is worse
        // than a misleading `last_status` — a 2xx arriving here would fail to
        // **reset** a running streak, so one misrouted call turns into a
        // persistent false alert that no later success clears.
        debug_assert!(
            !(200..300).contains(&status),
            "record_failure got success status {status}; a 2xx is record_success"
        );

        self.last_status = Some(status);

        let counts = match status {
            401 => true,
            403 if is_probe_label(label) => {
                // A permission the sell path does not need. Stamp when it
                // started and leave the counter alone.
                self.probe_403_since.get_or_insert(now);
                false
            }
            403 => true,
            _ => false,
        };

        if counts {
            self.consecutive_auth_failures = self.consecutive_auth_failures.saturating_add(1);
            if self.consecutive_auth_failures == 1 {
                self.first_failure_at = Some(now);
            }
        }
    }

    /// Is this provider's authentication failing badly enough to alert on?
    ///
    /// Both arms must hold. A clock that has moved backwards since the streak
    /// started yields `false` rather than an arbitrary duration — a system
    /// clock correction is not evidence of a revoked key.
    ///
    /// The mirror case is a known, accepted ceiling: `SystemTime` is
    /// NTP-steppable, so a large forward correction landing inside a young
    /// streak can satisfy the wall-clock arm early and let a key rotation
    /// alert. An `Instant` for the streak plus a `SystemTime` for display
    /// would be immune, at the cost of two clocks in one struct; a spurious
    /// alert during a clock step is a cheaper failure than the alternative.
    pub fn is_alerting(&self, now: SystemTime) -> bool {
        if self.consecutive_auth_failures < ALERT_MIN_CONSECUTIVE_FAILURES {
            return false;
        }
        match self.first_failure_at {
            Some(started) => now
                .duration_since(started)
                .map(|elapsed| elapsed >= ALERT_MIN_STREAK)
                .unwrap_or(false),
            // Unreachable while the type's own invariant holds: a counter of 1
            // or more always carries a `first_failure_at`. The arm exists
            // because every field is `pub`, so a struct literal built
            // elsewhere — or a row rehydrated from the eventual
            // `provider_health` table — can present a count with no start
            // time. Refusing to alert on a streak with no known start is the
            // safe reading of that.
            None => false,
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

    const HOT: &str = "zaprite.create_order";

    #[test]
    fn two_failures_do_not_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(3_600));

        assert_eq!(h.consecutive_auth_failures, 2);
        // An hour of failing is not enough on its own: the count arm holds.
        assert!(!h.is_alerting(t(3_600)));
    }

    #[test]
    fn three_failures_past_the_window_alert() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(300));
        h.record_failure(HOT, 401, t(601));

        assert_eq!(h.consecutive_auth_failures, 3);
        assert_eq!(h.first_failure_at, Some(t(0)));
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
        assert_eq!(h.consecutive_auth_failures, 3);
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

        assert_eq!(h.first_failure_at, Some(t(10)));
    }

    #[test]
    fn a_success_resets_the_streak() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(100));
        h.record_failure(HOT, 401, t(700));
        assert!(h.is_alerting(t(700)));

        h.record_success(HOT, t(800));

        assert_eq!(h.consecutive_auth_failures, 0);
        assert_eq!(h.first_failure_at, None);
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
        assert_eq!(h.consecutive_auth_failures, 2);
        assert_eq!(h.first_failure_at, Some(t(900)));
        assert!(!h.is_alerting(t(100_000)));
    }

    #[test]
    fn transport_and_server_errors_are_inert_in_both_directions() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));

        // A 5xx: neither counts toward the streak...
        h.record_failure(HOT, 503, t(120));
        assert_eq!(h.consecutive_auth_failures, 2);
        assert_eq!(h.first_failure_at, Some(t(0)));
        // ...nor resets it. It is only an observation.
        assert_eq!(h.last_status, Some(503));

        // Same for a 429, and for the statuses that are neither auth nor
        // server errors.
        h.record_failure(HOT, 429, t(180));
        h.record_failure(HOT, 404, t(240));
        assert_eq!(h.consecutive_auth_failures, 2);
        assert_eq!(h.first_failure_at, Some(t(0)));

        // A timeout, or a DNS/TLS failure, never reaches a status, so nothing
        // is recorded for it and there is deliberately no entry point to call
        // here. That leg is inert *by construction* rather than by assertion,
        // and no test can pin it; what this test does pin is the half that
        // could regress, a real status arriving and disturbing the streak.

        // The third genuine auth failure still tips it over, and it is the
        // third — the inert outcomes did not inflate the count either.
        h.record_failure(HOT, 401, t(700));
        assert_eq!(h.consecutive_auth_failures, 3);
        assert!(h.is_alerting(t(700)));
    }

    #[test]
    fn a_hot_path_403_counts() {
        // One tracker is one `payment_providers` row, so every label in a
        // single tracker comes from the same provider.
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 403, t(0));
        h.record_failure("zaprite.get_order", 403, t(300));
        h.record_failure("zaprite.charge_order_with_profile", 403, t(900));

        assert_eq!(h.consecutive_auth_failures, 3);
        // Whatever the cause of a 403 on the sell path, the operator cannot
        // sell, so it alerts.
        assert!(h.is_alerting(t(900)));
        assert_eq!(h.probe_403_since, None);
    }

    #[test]
    fn a_probe_403_does_not_count_and_is_recorded() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(0));
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(1_000));
        h.record_failure(BTCPAY_PROBE_LABEL, 403, t(2_000));

        // The probe touches a broader permission than the sell path, so a key
        // that sells fine can 403 here forever.
        assert_eq!(h.consecutive_auth_failures, 0);
        assert_eq!(h.first_failure_at, None);
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
    /// alert window is 10 — so a probe 403 that quietly cleared the streak
    /// would wipe every streak before it could mature, and a genuinely revoked
    /// key would alert *never* rather than late. Same failure shape as the
    /// withdrawn `ever_succeeded` flag.
    #[test]
    fn a_probe_403_does_not_disturb_a_running_streak() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));

        h.record_failure(ZAPRITE_PROBE_LABEL, 403, t(120));

        assert_eq!(h.consecutive_auth_failures, 2);
        assert_eq!(h.first_failure_at, Some(t(0)));
        assert_eq!(h.probe_403_since, Some(t(120)));

        h.record_failure(HOT, 401, t(180));
        assert_eq!(h.consecutive_auth_failures, 3);
        assert!(h.is_alerting(t(700)));
    }

    #[test]
    fn a_probe_401_counts_like_any_other_401() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(0));
        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(400));
        h.record_failure(ZAPRITE_PROBE_LABEL, 401, t(800));

        // The probe-label exclusion is about 403 only. 401 is the confirmed
        // revocation signal on both providers and counts everywhere — the
        // probe exists precisely to find a revoked key before a buyer does.
        assert_eq!(h.consecutive_auth_failures, 3);
        assert!(h.is_alerting(t(800)));
        // And it is filed as a revocation, not as a permissions gap: a
        // spurious `probe_403_since` here would put a "permissions"
        // observation next to a genuine revoked-key alert.
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

    /// The restart regression. StartOS restarts the daemon on every package
    /// update, and this map is in memory, so the very first call a fresh
    /// process observes can be the failing one.
    ///
    /// An earlier revision of the rule gated 403 handling on a runtime
    /// `ever_succeeded` flag. An already-revoked key can never flip such a flag
    /// true, so after any restart the alert could never fire again — a
    /// permanent miss, not a delayed one. This pins that the rule reads only
    /// the current call's label and status: a tracker with no history at all
    /// must still reach an alert.
    #[test]
    fn fresh_tracker_after_restart_still_alerts() {
        let mut h = ProviderAuthHealth::default();
        assert_eq!(h, ProviderAuthHealth::default());
        assert_eq!(h.last_success_at, None);

        h.record_failure("btcpay.create_invoice", 401, t(0));
        h.record_failure("btcpay.create_invoice", 401, t(500));
        h.record_failure("btcpay.get_invoice", 401, t(1_000));

        assert!(h.is_alerting(t(1_000)));

        // And the same holds for the hot-path 403 arm, the other half that the
        // withdrawn `ever_succeeded` flag would have suppressed.
        let mut h = ProviderAuthHealth::default();
        h.record_failure("btcpay.create_invoice", 403, t(0));
        h.record_failure("btcpay.create_invoice", 403, t(500));
        h.record_failure("btcpay.create_invoice", 403, t(1_000));
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
    /// the status and leaves the counter alone.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "record_failure got success status 200")]
    fn a_success_status_routed_into_record_failure_is_caught() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 200, t(0));
    }

    /// A probe success mid-streak resets the counter, exactly like any other
    /// success. **Pinned, not endorsed.** Whether a liveness probe coming back
    /// 200 should clear a streak of hot-path 401s is a semantic question the
    /// plan did not settle — the probe and the sell path can authenticate
    /// against different permissions — and it goes to the operator before the
    /// probe lands. Until then this test states today's behavior, so a change
    /// to it fails loudly and names exactly what moved instead of drifting.
    #[test]
    fn a_probe_success_currently_resets_a_running_streak() {
        let mut h = ProviderAuthHealth::default();
        h.record_failure(HOT, 401, t(0));
        h.record_failure(HOT, 401, t(60));
        assert_eq!(h.consecutive_auth_failures, 2);

        h.record_success(ZAPRITE_PROBE_LABEL, t(120));

        assert_eq!(h.consecutive_auth_failures, 0);
        assert_eq!(h.first_failure_at, None);
        assert_eq!(h.last_success_at, Some(t(120)));
        assert!(!h.is_alerting(t(100_000)));
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
