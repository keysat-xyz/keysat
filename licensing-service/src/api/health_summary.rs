//! Admin "is anything quietly broken?" summary.
//!
//! Keysat can fail in ways the operator never sees, because the thing that
//! would normally report the failure is the thing that broke. If the operator's
//! webhook receiver dies, Keysat's webhooks cannot report it. If the BTCPay or
//! Zaprite API key is revoked or de-scoped, nothing notices until a buyer
//! reaches checkout and cannot pay. This endpoint is the one place those
//! self-observations are collected and given a verdict.
//!
//! Shaped like [`super::db_info`] deliberately — `require_admin`, plain
//! `sqlx::query_scalar` aggregates, one `Json(json!(…))` — because it is the
//! same kind of thing: a read-only operator sanity check with no state of its
//! own.
//!
//! # Response shape
//!
//! ```jsonc
//! {
//!   "status": "ok" | "warn" | "critical",   // worst of the conditions below
//!   "generated_at": "<rfc3339>",
//!   "conditions": {
//!     "webhook_dead_letters": { "status": …, … },
//!     "payment_providers":    { "status": …, "providers": [ … ] }
//!   }
//! }
//! ```
//!
//! `conditions` is a **map, not an array**, so a condition can be added without
//! reshaping anything a consumer already reads. The overall `status` is the
//! worst of the per-condition statuses, ordered `ok < warn < critical`.
//!
//! # What each condition can and cannot tell you
//!
//! Both conditions are honest about their own imprecision, in the payload
//! rather than only in this comment, because the consumer that renders them
//! (the admin SPA card, and eventually the StartOS health check) will not be
//! reading this file. Every condition carries a `note` describing what the
//! measurement proves, and a `message` — non-null only when it is actually
//! unhappy — carrying the wording an operator should see.
//!
//! **`exact: false` is a contract on the renderer, not a remark.** It appears
//! on every object that has a `message`, and it means the message overclaims
//! when read alone, so `note` must be rendered *with* it rather than tucked
//! behind a tooltip. It is machine-readable precisely because the alternative
//! is a convention, and a card cannot bind to a convention.
//!
//! **No wording here may state that an API key was revoked.** A 401 is the
//! confirmed revocation signal on both providers, but it is also exactly what a
//! mistyped or half-rotated key looks like; and a 403 on BTCPay is scope, not
//! revocation. See [`crate::payment::health`], which is where that rule is
//! derived rather than merely restated.

use crate::api::admin::require_admin;
use crate::api::AppState;
use crate::db::repo;
use crate::error::AppResult;
use crate::payment::health::{FailureStreak, ProviderAuthHealth};
use crate::webhooks::{BAD_HMAC_KEY_ERROR_PREFIX, DISABLED_ENDPOINT_ERROR};
use axum::{extract::State, http::HeaderMap, Json};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// How far back a dead-lettered delivery still counts toward the alert.
///
/// Lifetime totals are reported alongside it, so nothing is hidden — this is
/// only about what turns the card yellow. Without a window, one bad afternoon
/// two years ago would leave the summary permanently unhappy and the operator
/// would learn to ignore it, which is the failure mode this whole endpoint
/// exists to avoid.
///
/// Measured against `created_at`, when the delivery was **enqueued**, because
/// there is no column recording when the worker gave up. Normally within four
/// hours of each other (the retry ladder tops out at about 3h55m), but
/// `repo::requeue_delivery` resets `attempt_count` without touching
/// `created_at`, so an old delivery an operator re-queued and which then died
/// again never re-enters the window. It stays visible in `alerting_total` and
/// `oldest_alerting_at`; the payload says "enqueued" rather than "failed" so
/// the number is not read as something it is not.
const DEAD_LETTER_WINDOW_DAYS: i64 = 7;

/// The dead-letter state itself: the delivery worker gave up.
///
/// `delivered_at IS NULL` (never succeeded) `AND next_attempt_at IS NULL` (not
/// scheduled for another try) `AND attempt_count > 0` (it was actually
/// attempted, so it is not a freshly-enqueued row). Matches
/// `repo::DeliveryStatusFilter::Failed`, which is what the admin delivery list
/// shows.
const DEAD_LETTER_PREDICATE: &str =
    "delivered_at IS NULL AND next_attempt_at IS NULL AND attempt_count > 0";

/// The two dead-letter causes that are the operator's own doing rather than a
/// receiver failing, and so must not raise an alert.
///
/// Three details here are load-bearing and each was a real trap:
///
/// 1. **`COALESCE`, not a bare comparison.** In SQLite `NULL <> 'x'` is `NULL`,
///    not true, so a row with no `last_error` would be silently dropped from
///    the alerting set by the very clause meant to *keep* it. No writer
///    produces such a row today; the coalesce is what stops a future one from
///    quietly losing the signal.
/// 2. **`LIKE`, not `=`.** `webhooks.rs` writes `bad HMAC key: {e}` — a
///    *prefix*, not a fixed string — so an equality test would never match
///    anything and the exclusion would be dead code that reads as if it works.
/// 3. **Both literals are bound from the constants the writer uses**
///    ([`DISABLED_ENDPOINT_ERROR`], [`BAD_HMAC_KEY_ERROR_PREFIX`]), so the
///    writer and this reader cannot drift apart. That is the same class of bug
///    as (2), just discovered later instead of never.
const INFORMATIONAL_EXCLUSIONS: &str = "COALESCE(last_error, '') <> ? \
     AND COALESCE(last_error, '') NOT LIKE ?";

/// What an alerting [`auth_dead`](ProviderAuthHealth::auth_dead) streak says to
/// an operator.
///
/// "may be revoked", never "revoked". The signal cannot tell a revoked key from
/// a mistyped or half-rotated one.
const AUTH_DEAD_MESSAGE: &str = "authentication failing (key may be revoked)";

/// What an alerting [`cannot_sell`](ProviderAuthHealth::cannot_sell) streak
/// says to an operator.
///
/// **This headline overclaims on its own, and [`CANNOT_SELL_NOTE`] is not
/// optional decoration next to it.** The streak counts a 403 from any non-probe
/// call, but only three of the nine calls Keysat makes actually create an
/// invoice — so a key that sells perfectly while missing `canviewinvoices`
/// raises it from the reconcile sweep. Any renderer of this field must render
/// the note with it, which is what `exact: false` says in the payload; see
/// [`streak_json`].
const CANNOT_SELL_MESSAGE: &str = "cannot create invoices (insufficient permissions)";

const AUTH_DEAD_NOTE: &str = "Counted from HTTP 401s on any provider call, and cleared by any \
     success. A 401 is the confirmed revocation signal on both providers, but a mistyped or \
     half-rotated key produces exactly the same 401, so this can only report that the key may \
     have been revoked. Alerts only after 3 consecutive failures spanning at least 10 minutes, \
     which is what keeps a key rotation from paging you for your own maintenance.";

const CANNOT_SELL_NOTE: &str = "Counted from HTTP 403s on any non-probe call, and cleared only by \
     a call that actually created an invoice. Only 3 of the 9 calls Keysat makes create one, so a \
     key that sells perfectly but is missing a read permission can raise this — check the key's \
     permissions rather than assuming checkout is broken. Because only a real sale clears it, read \
     first_failure_at and last_success_at together: on a quiet store this can mean no sale has \
     been attempted since that time, not that anything failed just now.";

const PERMISSIONS_NOTE: &str = "The liveness probe deliberately asks for a broader permission than \
     selling needs (BTCPay's canviewstoresettings, versus the cancreateinvoice a checkout uses), \
     so a key that sells perfectly well can be denied here forever. Recorded as an observation \
     and never alerts.";

const DEAD_LETTER_NOTE: &str = "Counts webhook deliveries that exhausted their retries and will \
     not be tried again, by the date they were enqueued. alerting_count covers the window named \
     by window_days; alerting_total, oldest_at, disabled_endpoint_total and bad_hmac_key_total \
     are all LIFETIME figures and are not windowed, so oldest_at can name a very old delivery \
     while status is ok. Two causes are excluded from the alerting set because they are the \
     operator's own configuration rather than a receiver failing: an endpoint that was deleted or \
     disabled (counted in disabled_endpoint_total) and an endpoint whose stored secret is \
     unusable as an HMAC key (counted in bad_hmac_key_total). Not exact: last_error is \
     last-write-wins, so disabling an endpoint part-way through a delivery's retry ladder \
     reclassifies a genuinely failing delivery as informational. It can only shrink the alerting \
     set, never grow it, which is why this set is deliberately narrower than the admin delivery \
     list's status=failed filter.";

/// Overall verdict for the summary and for each condition in it.
///
/// `Ord` is derived and the variant order is the severity order, so rolling
/// several conditions up is `.max()` and a condition added later joins in
/// without touching the roll-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Ok,
    Warn,
    Critical,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Critical => "critical",
        }
    }
}

/// `SystemTime` as RFC-3339, or JSON `null`.
fn ts(t: Option<SystemTime>) -> Value {
    match t {
        Some(t) => json!(DateTime::<Utc>::from(t).to_rfc3339()),
        None => Value::Null,
    }
}

/// One [`FailureStreak`] as JSON, plus whether it is alerting.
///
/// `first_failure_at` is always rendered, not just the count. On
/// [`cannot_sell`](ProviderAuthHealth::cannot_sell) that is the difference
/// between "checkout is broken right now" and "nothing has been sold since
/// Tuesday", and the count alone cannot distinguish them.
///
/// # `exact`
///
/// `exact: false` means **`message` overclaims when read on its own, and a
/// renderer must show `note` alongside it** — not behind a tooltip, not on a
/// details pane the operator may never open.
///
/// It is a field rather than a comment for one reason: the alternative is a
/// convention, and a convention cannot be bound to. A card that renders
/// `message` as its headline and hides `note` reproduces exactly the hazard
/// this endpoint's wording rules exist to prevent, and nothing would catch it.
/// With the flag in the payload, a renderer can branch on it and a test can
/// assert the branch.
fn streak_json(
    streak: &FailureStreak,
    now: SystemTime,
    message: &str,
    note: &str,
    exact: bool,
) -> (bool, Value) {
    let alerting = streak.is_alerting(now);
    (
        alerting,
        json!({
            "alerting": alerting,
            "consecutive": streak.consecutive,
            "first_failure_at": ts(streak.first_failure_at),
            // Non-null only when there is something to say, so a consumer can
            // render `message` directly without deciding whether to.
            "message": if alerting { json!(message) } else { Value::Null },
            "exact": exact,
            "note": note,
        }),
    )
}

/// One payment provider's row plus its observed health, as JSON.
fn provider_json(row: &repo::PaymentProviderRow, health: &ProviderAuthHealth, now: SystemTime) -> (Status, Value) {
    let (auth_dead_alerting, auth_dead) = streak_json(
        &health.auth_dead,
        now,
        AUTH_DEAD_MESSAGE,
        AUTH_DEAD_NOTE,
        // Exact: a 401 is an authentication failure on any label, and the
        // headline already carries the only qualifier the signal needs ("may
        // be revoked"). It claims nothing the streak does not measure.
        true,
    );
    let (cannot_sell_alerting, cannot_sell) = streak_json(
        &health.cannot_sell,
        now,
        CANNOT_SELL_MESSAGE,
        CANNOT_SELL_NOTE,
        // NOT exact: six of the nine labels that raise this streak create no
        // invoice, and the highest-volume one (reconcile's `get_invoice`)
        // needs a read permission, not `cancreateinvoice`. The headline is the
        // ratified wording; the qualifier lives in the note, so the note is
        // mandatory reading wherever the headline is shown.
        false,
    );

    let status = if auth_dead_alerting || cannot_sell_alerting {
        Status::Critical
    } else {
        Status::Ok
    };

    // "Nothing has been observed for this provider" is NOT "there is no map
    // entry": the liveness probe creates an entry by being *scheduled*, before
    // any status comes back. The reliable test is the pair of observation
    // fields. It is a neutral fact, not a fault — a daemon that has just
    // booted, or just connected a provider, looks exactly like this.
    let stale = health.last_status.is_none() && health.last_success_at.is_none();

    (
        status,
        json!({
            "id": row.id,
            "kind": row.kind,
            "label": row.label,
            "status": status.as_str(),
            "auth_dead": auth_dead,
            "cannot_sell": cannot_sell,
            "last_success_at": ts(health.last_success_at),
            // Deliberately reported *next to* the streaks and never in place of
            // them: any success clears this field, so an alerting `cannot_sell`
            // can sit beside `last_status: null` without contradiction. A
            // consumer that named the problem from this field would go quiet
            // exactly when the provider is half-working.
            "last_status": health.last_status,
            "permissions": {
                "limited": health.probe_403_since.is_some(),
                "since": ts(health.probe_403_since),
                // Constant, and stated in the payload rather than only in the
                // docs: a probe 403 is the documented normal case on BTCPay,
                // so a consumer must not colour the card on it.
                "alerting": false,
                "note": PERMISSIONS_NOTE,
            },
            "last_probe_at": ts(health.last_probe_at),
            "stale": stale,
        }),
    )
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;

    let now = SystemTime::now();
    let generated_at = Utc::now();

    // ---------------------------------------------------------------
    // Condition: webhook dead letters — five reported figures, two queries.
    //
    // `COALESCE(last_error, …)` is non-sargable and `webhook_deliveries` has no
    // index that could serve it, so each of these is a scan. One query per
    // figure was five scans of the same table for one page, and the admin card
    // will poll this. Folding the window into `SUM(created_at >= ?)` and the
    // two informational causes into two `SUM`s costs nothing extra per scan and
    // has a second benefit: figures that must agree are now computed over one
    // row set by construction rather than by three predicates staying in step.
    //
    // Runtime-prepared, not the compile-checked macro, so a bad column surfaces
    // only when the query runs — both have executing tests, per
    // `docs/guides/testing.md`.
    //
    // **Bind order is textual, not logical.** `?` placeholders bind
    // left-to-right through the whole statement including the SELECT list, so
    // `window_start` binds first here even though it reads like a filter.
    // `health_summary_dead_letter_aggregates_execute_and_classify` pins it:
    // every figure it asserts differs from the others, so a swapped bind
    // cannot pass.
    // ---------------------------------------------------------------
    let alerting_predicate = format!("{DEAD_LETTER_PREDICATE} AND {INFORMATIONAL_EXCLUSIONS}");
    let hmac_pattern = format!("{BAD_HMAC_KEY_ERROR_PREFIX}%");
    let window_start = (generated_at - ChronoDuration::days(DEAD_LETTER_WINDOW_DAYS)).to_rfc3339();

    // `oldest_at` is lifetime, deliberately: "your oldest undelivered webhook
    // is from March" is the useful reading, and a windowed MIN would only
    // restate the window. It can therefore name a very old row while `status`
    // is `ok`.
    let (alerting_count, alerting_total, oldest_at): (i64, i64, Option<String>) =
        sqlx::query_as(&format!(
            "SELECT COALESCE(SUM(created_at >= ?), 0), COUNT(*), MIN(created_at) \
             FROM webhook_deliveries WHERE {alerting_predicate}"
        ))
        .bind(&window_start)
        .bind(DISABLED_ENDPOINT_ERROR)
        .bind(&hmac_pattern)
        .fetch_one(&state.db)
        .await?;

    // The two informational causes, over the same dead-letter row set. Both
    // lifetime — named `_total`, like `alerting_total` and unlike
    // `alerting_count`, so nothing reads them as windowed.
    //
    // `bad_hmac_key_total` exists so the classification is closed: without it
    // those rows are excluded from the alert AND counted nowhere, so
    // `alerting_total + disabled_endpoint_total` cannot be reconciled against
    // the admin list's `status=failed`, and a permanently broken endpoint
    // secret would leave the summary reading green with nothing to explain it.
    let (disabled_endpoint_total, bad_hmac_key_total): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COALESCE(SUM(COALESCE(last_error, '') = ?), 0), \
                COALESCE(SUM(COALESCE(last_error, '') LIKE ?), 0) \
         FROM webhook_deliveries WHERE {DEAD_LETTER_PREDICATE}"
    ))
    .bind(DISABLED_ENDPOINT_ERROR)
    .bind(&hmac_pattern)
    .fetch_one(&state.db)
    .await?;

    // A lost webhook is the operator's integration being broken, not Keysat
    // being unable to take money — so it is a `warn` and never a `critical`.
    // Reserving red for "the daemon cannot do its job" is what keeps red
    // meaningful.
    let dead_letter_status = if alerting_count > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    let dead_letter_message = if alerting_count > 0 {
        let plural = if alerting_count == 1 {
            "delivery"
        } else {
            "deliveries"
        };
        json!(format!(
            "{alerting_count} webhook {plural} enqueued in the last {DEAD_LETTER_WINDOW_DAYS} \
             days gave up and will not be retried"
        ))
    } else {
        Value::Null
    };

    // ---------------------------------------------------------------
    // Condition: payment providers.
    // ---------------------------------------------------------------
    let rows = repo::list_all_payment_providers(&state.db).await?;

    // Prune on read — the contract `payment::health` documents. It removes
    // ONLY entries whose `payment_providers` row is gone: an entry also carries
    // the probe's throttle timestamp, so dropping a *live* provider's entry
    // would reset that throttle and re-fire its probe on the next 60-second
    // reconcile tick instead of on its 15-minute cadence.
    //
    // The row list is read before the lock is taken (it is `async`, and this is
    // a `std` lock), so a provider connected in that sub-millisecond gap has
    // its brand-new entry pruned. Harmless and self-correcting: the only thing
    // lost is a throttle stamp for a provider that has just been connected, so
    // at worst its first probe runs one reconcile tick early, once.
    let live_ids: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let snapshot: HashMap<String, ProviderAuthHealth> = {
        // One `write()`, scoped to this block, and there is no `.await` inside
        // it — this is a `std::sync::RwLock`, so holding a guard across a
        // suspension point is the one shape that could wedge the map. Poisoning
        // is recovered from rather than propagated, exactly as
        // `ProviderHealthSink::record` does: one panicking task must not take
        // the whole alert surface down with it.
        let mut guard = match state.provider_health.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain(|id, _| live_ids.contains(id.as_str()));
        guard.clone()
    };

    let default_health = ProviderAuthHealth::default();
    let mut provider_status = Status::Ok;
    let mut alerting_providers = 0_i64;
    let mut providers = Vec::with_capacity(rows.len());
    for row in &rows {
        let health = snapshot.get(&row.id).unwrap_or(&default_health);
        let (status, value) = provider_json(row, health, now);
        if status == Status::Critical {
            alerting_providers += 1;
        }
        provider_status = provider_status.max(status);
        providers.push(value);
    }

    let provider_message = if alerting_providers > 0 {
        let plural = if alerting_providers == 1 {
            "provider needs"
        } else {
            "providers need"
        };
        json!(format!(
            "{alerting_providers} payment {plural} attention — see the per-provider detail"
        ))
    } else {
        Value::Null
    };

    let overall = [dead_letter_status, provider_status]
        .into_iter()
        .max()
        .unwrap_or(Status::Ok);

    Ok(Json(json!({
        "status": overall.as_str(),
        "generated_at": generated_at.to_rfc3339(),
        "conditions": {
            "webhook_dead_letters": {
                "status": dead_letter_status.as_str(),
                "alerting_count": alerting_count,
                "window_days": DEAD_LETTER_WINDOW_DAYS,
                "alerting_total": alerting_total,
                "oldest_at": oldest_at,
                "disabled_endpoint_total": disabled_endpoint_total,
                "bad_hmac_key_total": bad_hmac_key_total,
                "message": dead_letter_message,
                // Same contract as a streak's `exact`: the headline count is
                // not the whole truth, so `note` must be rendered with it.
                "exact": false,
                "note": DEAD_LETTER_NOTE,
            },
            "payment_providers": {
                "status": provider_status.as_str(),
                "count": rows.len(),
                "alerting_count": alerting_providers,
                "message": provider_message,
                "providers": providers,
            },
        },
    })))
}
