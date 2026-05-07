//! Token-bucket rate limiting backed by SQLite.
//!
//! The state for each bucket lives in `rate_buckets` (bucket_kind, bucket_key).
//! Each incoming request refills the bucket based on wall-clock elapsed time
//! since last refill, then tries to spend one token. Returns `true` if the
//! request is allowed, `false` if it's rate-limited.
//!
//! Why store in SQLite instead of in-memory? Because the service is
//! single-tenant and small, and persisting lets us survive restarts without
//! giving attackers a "just bounce the process" bypass. The overhead of one
//! extra SQLite write per hit is negligible at our expected traffic.

use crate::error::AppResult;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

/// Try to spend one token from the given bucket. Returns `Ok(true)` if the
/// request is allowed, `Ok(false)` if rate-limited, or `Err` on a DB error.
///
/// - `capacity`: maximum tokens the bucket can hold (and what it starts at)
/// - `refill_per_second`: how many tokens to add per wall-clock second
pub async fn consume(
    pool: &SqlitePool,
    bucket_kind: &str,
    bucket_key: &str,
    capacity: f64,
    refill_per_second: f64,
) -> AppResult<bool> {
    let now = Utc::now();
    // Pull existing bucket, if any.
    let row = sqlx::query_as::<_, (f64, f64, f64, String)>(
        "SELECT tokens_remaining, capacity, refill_per_second, last_refill_at
         FROM rate_buckets WHERE bucket_kind = ? AND bucket_key = ?",
    )
    .bind(bucket_kind)
    .bind(bucket_key)
    .fetch_optional(pool)
    .await?;

    let (new_tokens, allowed) = match row {
        Some((prev_tokens, _cap, _refill, last_refill_at)) => {
            let last = DateTime::parse_from_rfc3339(&last_refill_at)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(now);
            let elapsed_s = (now - last).num_milliseconds() as f64 / 1000.0;
            let mut tokens = (prev_tokens + elapsed_s * refill_per_second).min(capacity);
            if tokens >= 1.0 {
                tokens -= 1.0;
                (tokens, true)
            } else {
                (tokens, false)
            }
        }
        None => {
            // Start with a full bucket minus the current request.
            (capacity - 1.0, true)
        }
    };

    let now_str = now.to_rfc3339();
    sqlx::query(
        "INSERT INTO rate_buckets
           (bucket_kind, bucket_key, tokens_remaining, capacity, refill_per_second, last_refill_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(bucket_kind, bucket_key) DO UPDATE SET
           tokens_remaining = excluded.tokens_remaining,
           capacity = excluded.capacity,
           refill_per_second = excluded.refill_per_second,
           last_refill_at = excluded.last_refill_at",
    )
    .bind(bucket_kind)
    .bind(bucket_key)
    .bind(new_tokens)
    .bind(capacity)
    .bind(refill_per_second)
    .bind(&now_str)
    .execute(pool)
    .await?;

    Ok(allowed)
}
