//! Admin "database health" snapshot.
//!
//! Cheap insurance against the catastrophic-loss risk: `/data/keysat.db`
//! is a single SQLite file, and losing it invalidates every license
//! ever issued by the daemon. StartOS's automated backup machinery
//! handles the actual snapshotting, but operators have nowhere to
//! see, at a glance, "what's in my DB right now and is it being
//! written to recently?" Without that, a half-failed restore or a
//! forgotten backup window goes unnoticed.
//!
//! This endpoint exposes:
//!   - DB file path + on-disk size in bytes
//!   - timestamp of the most recent write across audit_log,
//!     invoices, licenses (whichever moved last)
//!   - row counts across the operator-meaningful tables
//!
//! It does NOT report when StartOS last backed it up — the daemon
//! has no visibility into the host's snapshot subsystem. What it
//! gives the operator is a sanity check: "I expected ~50 licenses
//! and I see ~50 licenses; the file is N MB; the last write was 6
//! hours ago." If any of those numbers look wrong, that's a signal
//! to investigate before relying on a backup.

use crate::api::admin::require_admin;
use crate::api::AppState;
use crate::error::AppResult;
use axum::{extract::State, http::HeaderMap, Json};
use serde_json::{json, Value};

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;

    let db_path = state.config.db_path.clone();
    let db_size_bytes = std::fs::metadata(&db_path)
        .map(|m| m.len() as i64)
        .unwrap_or(-1);

    // Counts. UNION ALL into a single round-trip would be cute but
    // SQLite's COUNT-by-table doesn't share a query plan, so just
    // run the queries — they each take microseconds.
    let products: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await?;
    let policies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM policies")
        .fetch_one(&state.db)
        .await?;
    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await?;
    let active_licenses: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE status = 'active'")
            .fetch_one(&state.db)
            .await?;
    let invoices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
        .fetch_one(&state.db)
        .await?;
    let settled_invoices: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE status = 'settled'")
            .fetch_one(&state.db)
            .await?;
    let machines: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM machines WHERE deactivated_at IS NULL",
    )
    .fetch_one(&state.db)
    .await?;
    let discount_codes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discount_codes")
        .fetch_one(&state.db)
        .await?;
    let audit_log: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(&state.db)
        .await?;

    // Most recent write across the tables that get touched on
    // routine operator activity. ISO-8601 strings sort lexically.
    let last_write_at: Option<String> = sqlx::query_scalar(
        "SELECT MAX(t) FROM ( \
           SELECT MAX(occurred_at)  AS t FROM audit_log \
           UNION ALL SELECT MAX(updated_at) FROM invoices \
           UNION ALL SELECT MAX(issued_at)  FROM licenses \
         )",
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "db_path": db_path.display().to_string(),
        "db_size_bytes": db_size_bytes,
        "last_write_at": last_write_at,
        "counts": {
            "products": products,
            "policies": policies,
            "licenses_total": licenses,
            "licenses_active": active_licenses,
            "invoices_total": invoices,
            "invoices_settled": settled_invoices,
            "machines_active": machines,
            "discount_codes": discount_codes,
            "audit_log": audit_log,
        },
    })))
}
