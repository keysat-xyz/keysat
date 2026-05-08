//! Admin endpoints — all require `Authorization: Bearer <admin_api_key>`.
//! The operator uses these to manage products and issue/revoke licenses.

use crate::api::AppState;
use crate::crypto::{encode_key, sign_payload, LicensePayload, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Guards every admin handler: pulls the bearer token out of the header and
/// compares constant-time against the configured admin key. Returns the
/// SHA-256 hex of the token on success so handlers can write an audit row
/// that identifies *which* credential made the call without logging the raw
/// key.
///
/// Cookie-based session authentication is layered on top of this via the
/// `session_to_bearer_layer` axum middleware (see `crate::api::session_layer`):
/// when the SPA presents a valid `keysat_session` cookie, that middleware
/// injects an `Authorization: Bearer <api_key>` header on the way in, so
/// `require_admin` keeps working unchanged. The audit log limitation is
/// that all cookie-authenticated calls show the API key's sha256 as the
/// actor — IP / user-agent on the same row distinguish sessions in
/// practice. A v0.2 follow-up adds proper per-session actor identity.
pub fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<String> {
    let header_val = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let token = header_val
        .strip_prefix("Bearer ")
        .ok_or(AppError::Unauthorized)?;
    if bool::from(
        token
            .as_bytes()
            .ct_eq(state.config.admin_api_key.as_bytes()),
    ) {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        Ok(hex::encode(hasher.finalize()))
    } else {
        Err(AppError::Forbidden)
    }
}

/// Pull the best-effort client IP and User-Agent out of the request headers
/// for audit logging.
pub fn request_context(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    (client_ip, ua)
}

// ---------- Products ----------

#[derive(Debug, Deserialize)]
pub struct CreateProductReq {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Legacy SAT-only price. Optional now; if `price_currency` +
    /// `price_value` are supplied, they take precedence. Old SDK
    /// callers and the existing admin UI keep using this field
    /// without changes.
    #[serde(default)]
    pub price_sats: Option<i64>,
    /// New canonical currency. 'SAT' (default), 'USD', or 'EUR'.
    /// 'BTC' is intentionally not yet a separate currency code —
    /// pricing in BTC is just SAT pricing with a different display.
    /// Future v0.3+ may add it as a display alias.
    #[serde(default)]
    pub price_currency: Option<String>,
    /// Price in the smallest indivisible unit of `price_currency`:
    /// sats for SAT, cents for USD/EUR. Required when
    /// `price_currency` is supplied; ignored otherwise.
    #[serde(default)]
    pub price_value: Option<i64>,
    #[serde(default)]
    pub metadata: Value,
}

/// Currencies the admin endpoints accept. Whitelist enforced here so
/// a typo or future code error can't write a product with a bogus
/// currency tag that the daemon doesn't know how to convert.
const ACCEPTED_CURRENCIES: &[&str] = &["SAT", "USD", "EUR"];

/// Validate + normalize the request's price representation. Returns
/// `(currency, value_in_smallest_unit)`. Errors with 400 on:
///   - both `price_sats` and `price_currency` missing
///   - non-positive value
///   - unknown currency code
///   - both forms supplied with mismatched values (catches half-
///     migrated clients that send stale `price_sats` alongside a
///     fresh `price_value`)
fn resolve_price(req: &CreateProductReq) -> AppResult<(String, i64)> {
    match (req.price_currency.as_deref(), req.price_value, req.price_sats) {
        // Typed form — preferred.
        (Some(cur), Some(value), maybe_legacy) => {
            let cur = cur.to_uppercase();
            if !ACCEPTED_CURRENCIES.iter().any(|c| *c == cur) {
                return Err(AppError::BadRequest(format!(
                    "unsupported price_currency '{cur}'; accepted: {}",
                    ACCEPTED_CURRENCIES.join(", ")
                )));
            }
            if value <= 0 {
                return Err(AppError::BadRequest("price_value must be positive".into()));
            }
            // If the legacy field was ALSO sent, only accept it if
            // the currency is SAT and the numbers match. Anything
            // else means the client sent inconsistent state.
            if let Some(legacy) = maybe_legacy {
                if cur != "SAT" || legacy != value {
                    return Err(AppError::BadRequest(
                        "send price_currency + price_value, OR price_sats alone — \
                         not both with mismatched values".into(),
                    ));
                }
            }
            Ok((cur, value))
        }
        // Legacy form — back-compat.
        (None, None, Some(sats)) => {
            if sats <= 0 {
                return Err(AppError::BadRequest("price_sats must be positive".into()));
            }
            Ok(("SAT".to_string(), sats))
        }
        // Currency without value — incomplete.
        (Some(_), None, _) => Err(AppError::BadRequest(
            "price_currency was supplied but price_value is missing".into(),
        )),
        // Value without currency — ambiguous.
        (None, Some(_), _) => Err(AppError::BadRequest(
            "price_value was supplied but price_currency is missing".into(),
        )),
        // Nothing.
        (None, None, None) => Err(AppError::BadRequest(
            "must supply either price_sats (legacy) or price_currency + price_value".into(),
        )),
    }
}

pub async fn create_product(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProductReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    // Tier-cap gate: Creator caps at 5 products. 402 if over.
    crate::api::tier::enforce_product_cap(&state).await?;

    // Resolve the typed-currency form and the legacy form into a
    // single (currency, value) pair before hitting the repo. New
    // callers send price_currency + price_value; legacy callers
    // send price_sats alone; sending both is allowed only if the
    // currency is SAT and the values match (catches mismatched
    // updates from a half-migrated client).
    let (price_currency, price_value) = resolve_price(&req)?;
    let metadata = if req.metadata.is_null() {
        json!({})
    } else {
        req.metadata
    };
    let product = repo::create_product_with_currency(
        &state.db,
        &req.slug,
        &req.name,
        &req.description,
        &price_currency,
        price_value,
        &metadata,
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "product.create",
        Some("product"),
        Some(&product.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "slug": product.slug, "name": product.name, "price_sats": product.price_sats }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "product.created",
        &json!({ "product": product }),
    )
    .await;
    Ok(Json(json!(product)))
}

#[derive(Debug, Deserialize)]
pub struct SetActiveReq {
    pub active: bool,
}

/// Query options for product / policy delete.
#[derive(Debug, Deserialize)]
pub struct DeleteOpts {
    /// When true, cascades through every dependent row — licenses,
    /// invoices, discount-code redemptions, machines — instead of
    /// refusing with 409. Use only when tinkering or wiping pre-launch
    /// test data; in production this destroys customer history.
    #[serde(default)]
    pub force: bool,
}

/// Hard-delete a product. Two modes:
///
/// - **Safe (default)**: refuses if any invoice or license references
///   the product. Policies and unredeemed product-scoped codes are
///   cascade-deleted along with the product (templates only — no
///   audit-trail value on their own).
///
/// - **Force (`?force=true`)**: also wipes machines → discount
///   redemptions → licenses → invoices in dependency order before
///   removing the product. Destructive; reserved for testing /
///   pre-launch cleanup. Audit log records the cascade counts for
///   forensic backtracking.
pub async fn delete_product(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(opts): Query<DeleteOpts>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let product = repo::get_product_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{id}'")))?;

    let invoice_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE product_id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    let license_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE product_id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    if !opts.force && invoice_count + license_count > 0 {
        return Err(AppError::Conflict(format!(
            "cannot delete product '{}' — it has {} invoice(s) and {} license(s) \
             referencing it. Disable it instead (existing licenses keep working; \
             the product just stops being available for new purchases). To override \
             and wipe all references, use ?force=true.",
            product.slug, invoice_count, license_count
        )));
    }

    // Count what we'll cascade — informational, for the audit row + response.
    let policy_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM policies WHERE product_id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    let code_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM discount_codes WHERE applies_to_product_id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    let machine_count: i64 = if opts.force {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM machines WHERE license_id IN
             (SELECT id FROM licenses WHERE product_id = ?)",
        )
        .bind(&id)
        .fetch_one(&state.db)
        .await?
    } else {
        0
    };
    let redemption_count: i64 = if opts.force {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM discount_redemptions WHERE invoice_id IN
             (SELECT id FROM invoices WHERE product_id = ?)",
        )
        .bind(&id)
        .fetch_one(&state.db)
        .await?
    } else {
        0
    };

    // Cascade. Wrapped in a transaction so a partial failure leaves
    // consistent state.
    let mut tx = state.db.begin().await?;
    if opts.force {
        // Force: also wipe customer-history rows. Order matters — most
        // dependent rows first.
        sqlx::query(
            "DELETE FROM machines WHERE license_id IN
             (SELECT id FROM licenses WHERE product_id = ?)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM discount_redemptions WHERE invoice_id IN
             (SELECT id FROM invoices WHERE product_id = ?)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM licenses WHERE product_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM invoices WHERE product_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM discount_codes WHERE applies_to_product_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM policies WHERE product_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM products WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        if opts.force { "product.force_delete" } else { "product.delete" },
        Some("product"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "slug": product.slug,
            "name": product.name,
            "force": opts.force,
            "cascaded_policies": policy_count,
            "cascaded_codes": code_count,
            "cascaded_licenses": if opts.force { license_count } else { 0 },
            "cascaded_invoices": if opts.force { invoice_count } else { 0 },
            "cascaded_machines": machine_count,
            "cascaded_redemptions": redemption_count,
        }),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "deleted": product.slug,
        "force": opts.force,
        "cascaded_policies": policy_count,
        "cascaded_codes": code_count,
        "cascaded_licenses": if opts.force { license_count } else { 0 },
        "cascaded_invoices": if opts.force { invoice_count } else { 0 },
        "cascaded_machines": machine_count,
        "cascaded_redemptions": redemption_count,
    })))
}

/// Patch mutable fields on a product. Slug is NOT editable — it's part
/// of the public buy URL.
///
/// Two pricing forms accepted, mirroring the create endpoint:
/// - Legacy: `price_sats` alone (treated as a SAT-currency update).
/// - Typed:  `price_currency` + `price_value`. Either both or neither.
///   Sending a different currency than the product's current one
///   IS allowed — operators can convert a SAT product to USD pricing
///   in place. The daemon doesn't auto-recompute the sat-equivalent
///   for past invoices; future invoices use the new currency.
#[derive(Debug, Deserialize)]
pub struct UpdateProductReq {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub price_sats: Option<i64>,
    #[serde(default)]
    pub price_currency: Option<String>,
    #[serde(default)]
    pub price_value: Option<i64>,
}

pub async fn update_product(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateProductReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Resolve the pricing patch into (currency, value, sats) tuple
    // before passing to the repo. This mirrors the create-side
    // `resolve_price` validation so the same accept-both-forms
    // semantics apply on PATCH.
    let pricing_patch: Option<(String, i64)> = match (
        req.price_currency.as_deref(),
        req.price_value,
        req.price_sats,
    ) {
        // Typed form
        (Some(cur), Some(value), maybe_legacy) => {
            let cur = cur.to_uppercase();
            if !ACCEPTED_CURRENCIES.iter().any(|c| *c == cur) {
                return Err(AppError::BadRequest(format!(
                    "unsupported price_currency '{cur}'; accepted: {}",
                    ACCEPTED_CURRENCIES.join(", ")
                )));
            }
            if value < 0 {
                return Err(AppError::BadRequest("price_value must be >= 0".into()));
            }
            if let Some(legacy) = maybe_legacy {
                if cur != "SAT" || legacy != value {
                    return Err(AppError::BadRequest(
                        "send price_currency + price_value, OR price_sats alone — \
                         not both with mismatched values".into(),
                    ));
                }
            }
            Some((cur, value))
        }
        // Legacy SAT-only.
        (None, None, Some(sats)) => {
            if sats < 0 {
                return Err(AppError::BadRequest("price_sats must be >= 0".into()));
            }
            Some(("SAT".to_string(), sats))
        }
        (Some(_), None, _) => {
            return Err(AppError::BadRequest(
                "price_currency was supplied but price_value is missing".into(),
            ));
        }
        (None, Some(_), _) => {
            return Err(AppError::BadRequest(
                "price_value was supplied but price_currency is missing".into(),
            ));
        }
        // No pricing change — nothing to validate.
        (None, None, None) => None,
    };

    let updated = repo::update_product_with_currency(
        &state.db,
        &id,
        req.name.as_deref(),
        req.description.as_deref(),
        pricing_patch.as_ref().map(|(c, v)| (c.as_str(), *v)),
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "product.update",
        Some("product"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "name": req.name,
            "description": req.description,
            "price_sats": req.price_sats,
        }),
    )
    .await;
    Ok(Json(json!(updated)))
}

pub async fn set_product_active(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetActiveReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::set_product_active(&state.db, &id, req.active).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "product.set_active",
        Some("product"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Licenses ----------

#[derive(Debug, Deserialize)]
pub struct ListLicensesQuery {
    pub product_id: String,
}

pub async fn list_licenses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListLicensesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let licenses = repo::list_licenses_by_product(&state.db, &q.product_id).await?;
    Ok(Json(json!({ "licenses": licenses })))
}

#[derive(Debug, Deserialize)]
pub struct SearchLicensesQuery {
    pub buyer_email: Option<String>,
    pub nostr_npub: Option<String>,
    pub invoice_id: Option<String>,
}

/// Free-form lookup used by the "lost key recovery" flow. Searches by email,
/// Nostr npub, or invoice id (whichever is supplied), returns up to 100
/// matching licenses. With no filters supplied, returns the 100 most-recent
/// licenses (used by the admin UI's "recent licenses" default view).
///
/// Each row is hydrated with `policy_slug`, `policy_name`, and `product_slug`
/// so the admin UI can render those without extra round-trips.
pub async fn search_licenses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SearchLicensesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let licenses = repo::search_licenses(
        &state.db,
        q.buyer_email.as_deref(),
        q.nostr_npub.as_deref(),
        q.invoice_id.as_deref(),
    )
    .await?;

    // Hydrate with policy + product slugs. Two small lookup queries against
    // the unique ids referenced; cheap even for the 100-row max page.
    let policy_ids: Vec<String> = licenses
        .iter()
        .filter_map(|l| l.policy_id.clone())
        .collect();
    let product_ids: Vec<String> = licenses
        .iter()
        .map(|l| l.product_id.clone())
        .collect();

    let mut policy_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    if !policy_ids.is_empty() {
        let placeholders = vec!["?"; policy_ids.len()].join(",");
        let sql = format!("SELECT id, slug, name FROM policies WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
        for id in &policy_ids {
            q = q.bind(id);
        }
        for (id, slug, name) in q.fetch_all(&state.db).await? {
            policy_map.insert(id, (slug, name));
        }
    }

    let mut product_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if !product_ids.is_empty() {
        let placeholders = vec!["?"; product_ids.len()].join(",");
        let sql = format!("SELECT id, slug FROM products WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (String, String)>(&sql);
        for id in &product_ids {
            q = q.bind(id);
        }
        for (id, slug) in q.fetch_all(&state.db).await? {
            product_map.insert(id, slug);
        }
    }

    let enriched: Vec<Value> = licenses
        .into_iter()
        .map(|l| {
            let mut v = serde_json::to_value(&l).unwrap_or(json!({}));
            if let Some(pid) = &l.policy_id {
                if let Some((slug, name)) = policy_map.get(pid) {
                    v["policy_slug"] = json!(slug);
                    v["policy_name"] = json!(name);
                }
            }
            if let Some(slug) = product_map.get(&l.product_id) {
                v["product_slug"] = json!(slug);
            }
            v
        })
        .collect();

    Ok(Json(json!({ "licenses": enriched })))
}

/// Lifetime / 30d / 7d / 24h revenue from settled BTCPay invoices stored
/// locally. Powers the admin Overview "Revenue" stat card. Free-license
/// invoices have amount_sats = 0 and don't contribute. We deliberately
/// don't call the BTCPay API here — the local DB has every invoice we
/// ever created, including amount and status, so summing locally is
/// faster and works even if BTCPay is temporarily unreachable. (If we
/// ever want refunds / fees / chargebacks / Lightning vs on-chain
/// breakdown, that's when we'd hit BTCPay's API.)
pub async fn revenue_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_sats), 0) FROM invoices WHERE status = 'settled'",
    )
    .fetch_one(&state.db)
    .await?;
    let last_24h: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_sats), 0) FROM invoices
         WHERE status = 'settled' AND updated_at >= datetime('now','-24 hours')",
    )
    .fetch_one(&state.db)
    .await?;
    let last_7d: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_sats), 0) FROM invoices
         WHERE status = 'settled' AND updated_at >= datetime('now','-7 days')",
    )
    .fetch_one(&state.db)
    .await?;
    let last_30d: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_sats), 0) FROM invoices
         WHERE status = 'settled' AND updated_at >= datetime('now','-30 days')",
    )
    .fetch_one(&state.db)
    .await?;
    let settled_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE status = 'settled' AND amount_sats > 0")
            .fetch_one(&state.db)
            .await?;
    Ok(Json(json!({
        "total_sats": total,
        "last_24h_sats": last_24h,
        "last_7d_sats": last_7d,
        "last_30d_sats": last_30d,
        "settled_paid_invoice_count": settled_count,
    })))
}

/// License counts grouped by product_id and policy_id. Powers the
/// "X licenses" badge on the Products and Policies tables. Two small
/// COUNT-by-group queries; cheap to run on every Products/Policies route
/// open.
pub async fn license_counts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let by_product: Vec<(String, i64)> = sqlx::query_as(
        "SELECT product_id, COUNT(*) FROM licenses GROUP BY product_id",
    )
    .fetch_all(&state.db)
    .await?;
    let by_policy: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT policy_id, COUNT(*) FROM licenses GROUP BY policy_id",
    )
    .fetch_all(&state.db)
    .await?;
    let by_product_map: serde_json::Map<String, Value> = by_product
        .into_iter()
        .map(|(id, n)| (id, Value::from(n)))
        .collect();
    let by_policy_map: serde_json::Map<String, Value> = by_policy
        .into_iter()
        .filter_map(|(id, n)| id.map(|i| (i, Value::from(n))))
        .collect();
    Ok(Json(json!({
        "by_product": by_product_map,
        "by_policy": by_policy_map,
    })))
}

/// Aggregate counts for the admin Overview dashboard. Populates the
/// "Active licenses" stat card (and is small/cheap enough to query on
/// every dashboard load).
pub async fn licenses_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await?;
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE status = 'active'")
            .fetch_one(&state.db)
            .await?;
    let suspended: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE status = 'suspended'")
            .fetch_one(&state.db)
            .await?;
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE status = 'revoked'")
            .fetch_one(&state.db)
            .await?;
    let last_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM licenses WHERE issued_at >= datetime('now','-24 hours')",
    )
    .fetch_one(&state.db)
    .await?;
    let last_7d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM licenses WHERE issued_at >= datetime('now','-7 days')",
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "total": total,
        "active": active,
        "suspended": suspended,
        "revoked": revoked,
        "last_24h": last_24h,
        "last_7d": last_7d,
    })))
}

#[derive(Debug, Deserialize)]
pub struct IssueLicenseReq {
    pub product_slug: String,
    /// Optional policy slug (within the product). When set, the policy's
    /// duration, grace, entitlements, trial flag, and machine cap are used.
    #[serde(default)]
    pub policy_slug: Option<String>,
    /// Optional reason for audit — e.g. "comp", "press", "giveaway".
    #[serde(default)]
    pub note: Option<String>,
    /// Override expiry (ISO-8601 UTC). Ignored if `policy_slug` is set.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Override entitlements. Ignored if `policy_slug` is set.
    #[serde(default)]
    pub entitlements: Option<Vec<String>>,
    #[serde(default)]
    pub max_machines: Option<i64>,
    #[serde(default)]
    pub grace_seconds: Option<i64>,
    #[serde(default)]
    pub is_trial: Option<bool>,
    #[serde(default)]
    pub buyer_email: Option<String>,
    #[serde(default)]
    pub nostr_npub: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IssueLicenseResp {
    pub license_id: String,
    pub product_id: String,
    pub license_key: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub entitlements: Vec<String>,
    pub is_trial: bool,
    pub max_machines: i64,
}

/// Manually issue a license outside the purchase flow. Useful for comps,
/// press keys, grandfathered users, trial keys, or developer testing.
pub async fn issue_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IssueLicenseReq>,
) -> AppResult<Json<IssueLicenseResp>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let product = repo::get_product_by_slug(&state.db, &req.product_slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", req.product_slug)))?;

    // Pull the policy (if any) and merge it with per-call overrides.
    let policy = if let Some(slug) = &req.policy_slug {
        Some(
            repo::get_policy_by_slug(&state.db, &product.id, slug)
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!(
                        "policy '{slug}' for product '{}'",
                        req.product_slug
                    ))
                })?,
        )
    } else {
        None
    };

    // Compose effective values: explicit request fields take precedence over
    // the policy, which takes precedence over defaults.
    let now = Utc::now();
    let issued_at = now.to_rfc3339();
    let duration_seconds = policy.as_ref().map(|p| p.duration_seconds).unwrap_or(0);
    let expires_at = match (req.expires_at.clone(), duration_seconds) {
        (Some(explicit), _) => Some(explicit),
        (None, 0) => None, // perpetual
        (None, secs) => Some((now + Duration::seconds(secs)).to_rfc3339()),
    };
    let grace_seconds = req
        .grace_seconds
        .or_else(|| policy.as_ref().map(|p| p.grace_seconds))
        .unwrap_or(0);
    let max_machines = req
        .max_machines
        .or_else(|| policy.as_ref().map(|p| p.max_machines))
        .unwrap_or(1);
    let is_trial = req
        .is_trial
        .or_else(|| policy.as_ref().map(|p| p.is_trial))
        .unwrap_or(false);
    let entitlements = req
        .entitlements
        .clone()
        .or_else(|| policy.as_ref().map(|p| p.entitlements.clone()))
        .unwrap_or_default();

    let license_id = uuid::Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &issued_at,
        &json!({
            "source": "admin_issue",
            "note": req.note,
        }),
        policy.as_ref().map(|p| p.id.as_str()),
        expires_at.as_deref(),
        grace_seconds,
        max_machines,
        &entitlements,
        is_trial,
        req.buyer_email.as_deref(),
        req.nostr_npub.as_deref(),
    )
    .await?;

    // Build v2 signed payload.
    let mut flags = 0u8;
    if is_trial {
        flags |= crate::crypto::FLAG_TRIAL;
    }
    let payload = LicensePayload {
        version: KEY_VERSION_V2,
        flags,
        product_id: uuid::Uuid::parse_str(&product.id).unwrap(),
        license_id: uuid::Uuid::parse_str(&license_id).unwrap(),
        issued_at: now.timestamp(),
        expires_at: expires_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0),
        fingerprint_hash: [0u8; 32],
        entitlements: entitlements.clone(),
    };
    let sig = sign_payload(&state.keypair.signing, &payload);
    let license_key = encode_key(&payload, &sig);

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.issue_manual",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "product_id": product.id,
            "policy_id": policy.as_ref().map(|p| &p.id),
            "is_trial": is_trial,
            "expires_at": expires_at,
            "entitlements": entitlements,
        }),
    )
    .await;

    crate::webhooks::dispatch(
        &state,
        "license.issued",
        &json!({
            "license_id": license_id,
            "product_id": product.id,
            "is_trial": is_trial,
            "expires_at": expires_at,
            "entitlements": entitlements,
            "source": "admin_issue",
        }),
    )
    .await;

    Ok(Json(IssueLicenseResp {
        license_id,
        product_id: product.id,
        license_key,
        issued_at,
        expires_at,
        entitlements,
        is_trial,
        max_machines,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RevokeReq {
    #[serde(default)]
    pub reason: String,
}

pub async fn revoke_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
    Json(req): Json<RevokeReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let reason = if req.reason.is_empty() {
        "admin revoke".to_string()
    } else {
        req.reason
    };
    repo::revoke_license(&state.db, &license_id, &reason).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.revoke",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "reason": reason }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.revoked",
        &json!({ "license_id": license_id, "reason": reason }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Suspension / un-suspension ----------

#[derive(Debug, Deserialize)]
pub struct SuspendReq {
    #[serde(default)]
    pub reason: String,
}

pub async fn suspend_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
    Json(req): Json<SuspendReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let reason = if req.reason.is_empty() {
        "admin suspend".to_string()
    } else {
        req.reason
    };
    repo::suspend_license(&state.db, &license_id, &reason).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.suspend",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "reason": reason }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.suspended",
        &json!({ "license_id": license_id, "reason": reason }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn unsuspend_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::unsuspend_license(&state.db, &license_id).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.unsuspend",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.unsuspended",
        &json!({ "license_id": license_id }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Audit log viewer ----------

#[derive(Debug, Deserialize)]
pub struct ListAuditQuery {
    #[serde(default = "default_audit_limit")]
    pub limit: i64,
    pub action: Option<String>,
}

fn default_audit_limit() -> i64 {
    200
}

pub async fn list_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListAuditQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let rows = repo::list_audit(&state.db, q.limit.min(1000).max(1), q.action.as_deref()).await?;
    Ok(Json(json!({ "entries": rows })))
}

// ---------- Settings (live-mutable runtime config) ----------

/// Settings key for the operator's public-facing display name. Read by
/// the `/` index handler on every request, so updates take effect
/// immediately — no daemon restart needed.
pub const SETTING_OPERATOR_NAME: &str = "operator_name";

#[derive(Debug, Deserialize)]
pub struct SetOperatorNameReq {
    /// New operator name. Empty string clears the setting (reverts to
    /// the daemon's startup-time fallback from KEYSAT_OPERATOR_NAME).
    pub name: String,
}

pub async fn set_operator_name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetOperatorNameReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let trimmed = req.name.trim();
    let stored: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
    repo::settings_set(&state.db, SETTING_OPERATOR_NAME, stored).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "operator_name.set",
        Some("setting"),
        Some(SETTING_OPERATOR_NAME),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "value": stored }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "operator_name": stored })))
}

pub async fn get_operator_name(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let stored = repo::settings_get(&state.db, SETTING_OPERATOR_NAME).await?;
    let effective = stored
        .clone()
        .or_else(|| state.config.operator_name.clone());
    Ok(Json(json!({
        "stored": stored,
        "effective": effective,
        "fallback_env": state.config.operator_name,
    })))
}
