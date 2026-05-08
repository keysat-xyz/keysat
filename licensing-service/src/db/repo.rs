//! Repository helpers. Every query the app issues goes through here so the
//! SQL is centralized and the rest of the code stays storage-agnostic.

use crate::error::{AppError, AppResult};
use crate::models::{
    AuditEntry, DiscountCode, DiscountRedemption, Invoice, License, Machine, Policy, Product,
    WebhookDelivery, WebhookEndpoint,
};
use chrono::Utc;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

// ---------- Products ----------

pub async fn list_products(pool: &SqlitePool, only_active: bool) -> AppResult<Vec<Product>> {
    let q = if only_active {
        "SELECT id, slug, name, description, price_sats, active, metadata_json, created_at, updated_at
         FROM products WHERE active = 1 ORDER BY name"
    } else {
        "SELECT id, slug, name, description, price_sats, active, metadata_json, created_at, updated_at
         FROM products ORDER BY name"
    };
    let rows = sqlx::query(q).fetch_all(pool).await?;
    rows.into_iter().map(row_to_product).collect()
}

pub async fn get_product_by_slug(pool: &SqlitePool, slug: &str) -> AppResult<Option<Product>> {
    let row = sqlx::query(
        "SELECT id, slug, name, description, price_sats, active, metadata_json, created_at, updated_at
         FROM products WHERE slug = ?",
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_product).transpose()
}

pub async fn get_product_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<Product>> {
    let row = sqlx::query(
        "SELECT id, slug, name, description, price_sats, active, metadata_json, created_at, updated_at
         FROM products WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_product).transpose()
}

pub async fn create_product(
    pool: &SqlitePool,
    slug: &str,
    name: &str,
    description: &str,
    price_sats: i64,
    metadata: &serde_json::Value,
) -> AppResult<Product> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let metadata_json = serde_json::to_string(metadata)
        .map_err(|e| AppError::BadRequest(format!("invalid metadata JSON: {e}")))?;

    sqlx::query(
        "INSERT INTO products (id, slug, name, description, price_sats, active, metadata_json, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(&id)
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(price_sats)
    .bind(&metadata_json)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("product slug '{slug}' already exists"))
        }
        other => AppError::Database(other),
    })?;

    get_product_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created product not found")))
}

pub async fn set_product_active(pool: &SqlitePool, id: &str, active: bool) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query("UPDATE products SET active = ?, updated_at = ? WHERE id = ?")
        .bind(active as i64)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("product {id}")));
    }
    Ok(())
}

/// Patch mutable fields on a product. `slug` and `id` are intentionally
/// not editable — slug is part of the public buy URL, and changing it
/// would break links operators have shared. Each Option is "Some →
/// update, None → leave alone."
pub async fn update_product(
    pool: &SqlitePool,
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
    price_sats: Option<i64>,
) -> AppResult<Product> {
    let mut sets: Vec<&str> = Vec::new();
    if name.is_some() {
        sets.push("name = ?");
    }
    if description.is_some() {
        sets.push("description = ?");
    }
    if price_sats.is_some() {
        sets.push("price_sats = ?");
    }
    if sets.is_empty() {
        return get_product_by_id(pool, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product {id}")));
    }
    sets.push("updated_at = ?");
    let now = Utc::now().to_rfc3339();
    let sql = format!("UPDATE products SET {} WHERE id = ?", sets.join(", "));
    let mut q = sqlx::query(&sql);
    if let Some(v) = name {
        q = q.bind(v);
    }
    if let Some(v) = description {
        q = q.bind(v);
    }
    if let Some(v) = price_sats {
        q = q.bind(v);
    }
    q = q.bind(&now).bind(id);
    let rows = q.execute(pool).await?.rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("product {id}")));
    }
    get_product_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product {id}")))
}

fn row_to_product(row: sqlx::sqlite::SqliteRow) -> AppResult<Product> {
    let metadata_json: String = row.try_get("metadata_json")?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap_or_default();
    let active_int: i64 = row.try_get("active")?;
    Ok(Product {
        id: row.try_get("id")?,
        slug: row.try_get("slug")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        price_sats: row.try_get("price_sats")?,
        active: active_int != 0,
        metadata,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ---------- Invoices ----------

#[allow(clippy::too_many_arguments)]
pub async fn create_invoice(
    pool: &SqlitePool,
    id: &str,
    btcpay_invoice_id: &str,
    product_id: &str,
    amount_sats: i64,
    checkout_url: &str,
    buyer_email: Option<&str>,
    buyer_note: Option<&str>,
    policy_id: Option<&str>,
) -> AppResult<Invoice> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO invoices
         (id, btcpay_invoice_id, product_id, status, buyer_email, buyer_note,
          amount_sats, checkout_url, policy_id, created_at, updated_at)
         VALUES (?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(btcpay_invoice_id)
    .bind(product_id)
    .bind(buyer_email)
    .bind(buyer_note)
    .bind(amount_sats)
    .bind(checkout_url)
    .bind(policy_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;

    get_invoice_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created invoice not found")))
}

/// Synthesize a "settled" invoice with amount = 0 for free-license code
/// redemptions. Doesn't talk to BTCPay — used to keep the data model
/// uniform (every license still points at an invoice, every redemption
/// still points at an invoice). The btcpay_invoice_id is namespaced
/// `free-<uuid>` so it's distinguishable in audit and queries.
pub async fn create_free_invoice(
    pool: &SqlitePool,
    product_id: &str,
    buyer_email: Option<&str>,
    buyer_note: Option<&str>,
    policy_id: Option<&str>,
) -> AppResult<Invoice> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let synthetic_btcpay_id = format!("free-{id}");
    sqlx::query(
        "INSERT INTO invoices
         (id, btcpay_invoice_id, product_id, status, buyer_email, buyer_note,
          amount_sats, checkout_url, policy_id, created_at, updated_at)
         VALUES (?, ?, ?, 'settled', ?, ?, 0, '', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&synthetic_btcpay_id)
    .bind(product_id)
    .bind(buyer_email)
    .bind(buyer_note)
    .bind(policy_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_invoice_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("synthetic invoice not found")))
}

pub async fn get_invoice_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<Invoice>> {
    let row = sqlx::query(
        "SELECT id, btcpay_invoice_id, product_id, status, buyer_email, buyer_note,
                amount_sats, checkout_url, created_at, updated_at, policy_id
         FROM invoices WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_invoice))
}

pub async fn get_invoice_by_btcpay_id(
    pool: &SqlitePool,
    btcpay_invoice_id: &str,
) -> AppResult<Option<Invoice>> {
    let row = sqlx::query(
        "SELECT id, btcpay_invoice_id, product_id, status, buyer_email, buyer_note,
                amount_sats, checkout_url, created_at, updated_at, policy_id
         FROM invoices WHERE btcpay_invoice_id = ?",
    )
    .bind(btcpay_invoice_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_invoice))
}

pub async fn update_invoice_status(
    pool: &SqlitePool,
    btcpay_invoice_id: &str,
    status: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE invoices SET status = ?, updated_at = ? WHERE btcpay_invoice_id = ?",
    )
    .bind(status)
    .bind(&now)
    .bind(btcpay_invoice_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// List invoices still in `pending` status, created within the last
/// `max_age_hours` hours. Used by the reconciliation loop to catch up on
/// dropped webhooks.
pub async fn list_pending_invoices(
    pool: &SqlitePool,
    max_age_hours: i64,
) -> AppResult<Vec<Invoice>> {
    let cutoff = (Utc::now() - chrono::Duration::hours(max_age_hours)).to_rfc3339();
    let rows = sqlx::query(
        "SELECT id, btcpay_invoice_id, product_id, status, buyer_email, buyer_note,
                amount_sats, checkout_url, created_at, updated_at, policy_id
         FROM invoices
         WHERE status = 'pending' AND created_at >= ?
         ORDER BY created_at ASC",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_invoice).collect())
}

fn row_to_invoice(row: sqlx::sqlite::SqliteRow) -> Invoice {
    Invoice {
        id: row.get("id"),
        btcpay_invoice_id: row.get("btcpay_invoice_id"),
        product_id: row.get("product_id"),
        status: row.get("status"),
        buyer_email: row.get("buyer_email"),
        buyer_note: row.get("buyer_note"),
        amount_sats: row.get("amount_sats"),
        checkout_url: row.get("checkout_url"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        policy_id: row.try_get("policy_id").ok().flatten(),
    }
}

// ---------- Licenses ----------

const LICENSE_COLS: &str = "id, product_id, invoice_id, status, fingerprint, bound_identity,
                            issued_at, revoked_at, revocation_reason, metadata_json,
                            policy_id, expires_at, grace_seconds, max_machines,
                            suspended_at, suspension_reason, entitlements_json,
                            is_trial, nostr_npub, buyer_email";

#[allow(clippy::too_many_arguments)]
pub async fn create_license(
    pool: &SqlitePool,
    id: &str,
    product_id: &str,
    invoice_id: Option<&str>,
    issued_at: &str,
    metadata: &serde_json::Value,
    policy_id: Option<&str>,
    expires_at: Option<&str>,
    grace_seconds: i64,
    max_machines: i64,
    entitlements: &[String],
    is_trial: bool,
    buyer_email: Option<&str>,
    nostr_npub: Option<&str>,
) -> AppResult<License> {
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".into());
    let entitlements_json = serde_json::to_string(entitlements).unwrap_or_else(|_| "[]".into());
    sqlx::query(
        "INSERT INTO licenses
           (id, product_id, invoice_id, status, issued_at, metadata_json,
            policy_id, expires_at, grace_seconds, max_machines,
            entitlements_json, is_trial, buyer_email, nostr_npub)
         VALUES (?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(product_id)
    .bind(invoice_id)
    .bind(issued_at)
    .bind(&metadata_json)
    .bind(policy_id)
    .bind(expires_at)
    .bind(grace_seconds)
    .bind(max_machines)
    .bind(&entitlements_json)
    .bind(is_trial as i64)
    .bind(buyer_email)
    .bind(nostr_npub)
    .execute(pool)
    .await?;
    get_license_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created license not found")))
}

pub async fn get_license_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<License>> {
    let row = sqlx::query(&format!("SELECT {LICENSE_COLS} FROM licenses WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_license))
}

pub async fn get_license_by_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
) -> AppResult<Option<License>> {
    let row = sqlx::query(&format!(
        "SELECT {LICENSE_COLS} FROM licenses WHERE invoice_id = ?"
    ))
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_license))
}

pub async fn list_licenses_by_product(
    pool: &SqlitePool,
    product_id: &str,
) -> AppResult<Vec<License>> {
    let rows = sqlx::query(&format!(
        "SELECT {LICENSE_COLS} FROM licenses WHERE product_id = ? ORDER BY issued_at DESC"
    ))
    .bind(product_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_license).collect())
}

pub async fn search_licenses(
    pool: &SqlitePool,
    buyer_email: Option<&str>,
    nostr_npub: Option<&str>,
    invoice_id: Option<&str>,
) -> AppResult<Vec<License>> {
    // Build a simple dynamic WHERE — each filter is ANDed. If none provided,
    // returns the 100 most recent.
    let mut where_clauses: Vec<&str> = Vec::new();
    if buyer_email.is_some() {
        where_clauses.push("buyer_email = ?");
    }
    if nostr_npub.is_some() {
        where_clauses.push("nostr_npub = ?");
    }
    if invoice_id.is_some() {
        where_clauses.push("invoice_id = ?");
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT {LICENSE_COLS} FROM licenses {where_sql} ORDER BY issued_at DESC LIMIT 100"
    );
    let mut q = sqlx::query(&sql);
    if let Some(v) = buyer_email {
        q = q.bind(v);
    }
    if let Some(v) = nostr_npub {
        q = q.bind(v);
    }
    if let Some(v) = invoice_id {
        q = q.bind(v);
    }
    let rows = q.fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_license).collect())
}

pub async fn bind_fingerprint_if_unset(
    pool: &SqlitePool,
    license_id: &str,
    fingerprint: &str,
) -> AppResult<()> {
    // Only binds if the column is currently NULL — preserves first-use lock.
    // This is still maintained alongside the richer machines table for
    // backwards compatibility with single-seat licenses where the old
    // `licenses.fingerprint` column is the easiest check.
    sqlx::query(
        "UPDATE licenses SET fingerprint = ? WHERE id = ? AND fingerprint IS NULL",
    )
    .bind(fingerprint)
    .bind(license_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke_license(
    pool: &SqlitePool,
    license_id: &str,
    reason: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE licenses SET status = 'revoked', revoked_at = ?, revocation_reason = ?
         WHERE id = ? AND status != 'revoked'",
    )
    .bind(&now)
    .bind(reason)
    .bind(license_id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "license {license_id} (already revoked or does not exist)"
        )));
    }
    Ok(())
}

pub async fn suspend_license(
    pool: &SqlitePool,
    license_id: &str,
    reason: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE licenses SET status = 'suspended', suspended_at = ?, suspension_reason = ?
         WHERE id = ? AND status = 'active'",
    )
    .bind(&now)
    .bind(reason)
    .bind(license_id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "active license {license_id}"
        )));
    }
    Ok(())
}

pub async fn unsuspend_license(pool: &SqlitePool, license_id: &str) -> AppResult<()> {
    let rows = sqlx::query(
        "UPDATE licenses SET status = 'active', suspended_at = NULL, suspension_reason = NULL
         WHERE id = ? AND status = 'suspended'",
    )
    .bind(license_id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "suspended license {license_id}"
        )));
    }
    Ok(())
}

fn row_to_license(row: sqlx::sqlite::SqliteRow) -> License {
    let metadata_json: String = row.get("metadata_json");
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap_or_default();
    let entitlements_json: String = row
        .try_get("entitlements_json")
        .unwrap_or_else(|_| "[]".to_string());
    let entitlements: Vec<String> =
        serde_json::from_str(&entitlements_json).unwrap_or_default();
    let grace_seconds: i64 = row.try_get("grace_seconds").unwrap_or(0);
    let max_machines: i64 = row.try_get("max_machines").unwrap_or(1);
    let is_trial_int: i64 = row.try_get("is_trial").unwrap_or(0);
    License {
        id: row.get("id"),
        product_id: row.get("product_id"),
        invoice_id: row.get("invoice_id"),
        status: row.get("status"),
        fingerprint: row.get("fingerprint"),
        bound_identity: row.get("bound_identity"),
        issued_at: row.get("issued_at"),
        revoked_at: row.get("revoked_at"),
        revocation_reason: row.get("revocation_reason"),
        metadata,
        policy_id: row.try_get("policy_id").ok().flatten(),
        expires_at: row.try_get("expires_at").ok().flatten(),
        grace_seconds,
        max_machines,
        suspended_at: row.try_get("suspended_at").ok().flatten(),
        suspension_reason: row.try_get("suspension_reason").ok().flatten(),
        entitlements,
        is_trial: is_trial_int != 0,
        nostr_npub: row.try_get("nostr_npub").ok().flatten(),
        buyer_email: row.try_get("buyer_email").ok().flatten(),
    }
}

// ---------- Validation audit log ----------

#[allow(clippy::too_many_arguments)]
pub async fn log_validation(
    pool: &SqlitePool,
    license_id: Option<&str>,
    product_id: Option<&str>,
    fingerprint: Option<&str>,
    result: &str,
    client_ip: Option<&str>,
    user_agent: Option<&str>,
    machine_id: Option<&str>,
    reason_detail: Option<&str>,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO validation_log
         (license_id, product_id, fingerprint, result, client_ip, user_agent, occurred_at,
          machine_id, reason_detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(license_id)
    .bind(product_id)
    .bind(fingerprint)
    .bind(result)
    .bind(client_ip)
    .bind(user_agent)
    .bind(&now)
    .bind(machine_id)
    .bind(reason_detail)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------- Policies ----------

const POLICY_COLS: &str = "id, product_id, name, slug, duration_seconds, grace_seconds,
                           tip_recipient, tip_pct_bps, tip_label,
                           max_machines, is_trial, price_sats_override,
                           entitlements_json, metadata_json, active, public,
                           created_at, updated_at";

#[allow(clippy::too_many_arguments)]
pub async fn create_policy(
    pool: &SqlitePool,
    product_id: &str,
    name: &str,
    slug: &str,
    duration_seconds: i64,
    grace_seconds: i64,
    max_machines: i64,
    is_trial: bool,
    price_sats_override: Option<i64>,
    entitlements: &[String],
    metadata: &serde_json::Value,
    tip_recipient: Option<&str>,
    tip_pct_bps: i64,
    tip_label: Option<&str>,
) -> AppResult<Policy> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let entitlements_json = serde_json::to_string(entitlements).unwrap_or_else(|_| "[]".into());
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".into());
    let tip_pct = tip_pct_bps.clamp(0, 10_000);
    // public defaults to 1 here; admin can flip via PATCH /v1/admin/policies/:id/public.
    sqlx::query(
        "INSERT INTO policies
           (id, product_id, name, slug, duration_seconds, grace_seconds, max_machines,
            is_trial, price_sats_override, entitlements_json, metadata_json, active, public,
            tip_recipient, tip_pct_bps, tip_label, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(product_id)
    .bind(name)
    .bind(slug)
    .bind(duration_seconds)
    .bind(grace_seconds)
    .bind(max_machines)
    .bind(is_trial as i64)
    .bind(price_sats_override)
    .bind(&entitlements_json)
    .bind(&metadata_json)
    .bind(tip_recipient)
    .bind(tip_pct)
    .bind(tip_label)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("policy slug '{slug}' already exists for this product"))
        }
        other => AppError::Database(other),
    })?;
    get_policy_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created policy not found")))
}

pub async fn get_policy_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<Policy>> {
    let row = sqlx::query(&format!("SELECT {POLICY_COLS} FROM policies WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_policy))
}

pub async fn get_policy_by_slug(
    pool: &SqlitePool,
    product_id: &str,
    slug: &str,
) -> AppResult<Option<Policy>> {
    let row = sqlx::query(&format!(
        "SELECT {POLICY_COLS} FROM policies WHERE product_id = ? AND slug = ?"
    ))
    .bind(product_id)
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_policy))
}

pub async fn list_policies_by_product(
    pool: &SqlitePool,
    product_id: &str,
    only_active: bool,
) -> AppResult<Vec<Policy>> {
    let sql = if only_active {
        format!("SELECT {POLICY_COLS} FROM policies WHERE product_id = ? AND active = 1 ORDER BY name")
    } else {
        format!("SELECT {POLICY_COLS} FROM policies WHERE product_id = ? ORDER BY name")
    };
    let rows = sqlx::query(&sql).bind(product_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_policy).collect())
}

/// Public-buyer view: only active+public policies. Sorted by ascending
/// effective price so the cheapest tier renders leftmost. The buy page
/// is the only caller; admin should use `list_policies_by_product`.
pub async fn list_public_policies_by_product(
    pool: &SqlitePool,
    product_id: &str,
) -> AppResult<Vec<Policy>> {
    let sql = format!(
        "SELECT {POLICY_COLS} FROM policies
         WHERE product_id = ? AND active = 1 AND public = 1
         ORDER BY COALESCE(price_sats_override, 0) ASC, name ASC"
    );
    let rows = sqlx::query(&sql).bind(product_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_policy).collect())
}

/// Patch mutable fields on a policy. Slug, product_id, and id are
/// intentionally not editable — they're identifiers that operators may
/// have hard-coded into integration docs or buy URLs. Tip-related fields
/// have their own admin endpoint (`set_policy_tip_config`) since they
/// have their own validation rules (basis points, paired recipient/pct).
#[allow(clippy::too_many_arguments)]
pub async fn update_policy(
    pool: &SqlitePool,
    id: &str,
    name: Option<&str>,
    duration_seconds: Option<i64>,
    grace_seconds: Option<i64>,
    max_machines: Option<i64>,
    is_trial: Option<bool>,
    price_sats_override: Option<Option<i64>>,
    entitlements: Option<&[String]>,
    metadata: Option<&serde_json::Value>,
) -> AppResult<Policy> {
    let mut sets: Vec<&str> = Vec::new();
    if name.is_some() {
        sets.push("name = ?");
    }
    if duration_seconds.is_some() {
        sets.push("duration_seconds = ?");
    }
    if grace_seconds.is_some() {
        sets.push("grace_seconds = ?");
    }
    if max_machines.is_some() {
        sets.push("max_machines = ?");
    }
    if is_trial.is_some() {
        sets.push("is_trial = ?");
    }
    if price_sats_override.is_some() {
        sets.push("price_sats_override = ?");
    }
    if entitlements.is_some() {
        sets.push("entitlements_json = ?");
    }
    if metadata.is_some() {
        sets.push("metadata_json = ?");
    }
    if sets.is_empty() {
        return get_policy_by_id(pool, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("policy {id}")));
    }
    sets.push("updated_at = ?");
    let now = Utc::now().to_rfc3339();
    let sql = format!("UPDATE policies SET {} WHERE id = ?", sets.join(", "));
    let mut q = sqlx::query(&sql);
    if let Some(v) = name {
        q = q.bind(v);
    }
    if let Some(v) = duration_seconds {
        q = q.bind(v);
    }
    if let Some(v) = grace_seconds {
        q = q.bind(v);
    }
    if let Some(v) = max_machines {
        q = q.bind(v);
    }
    if let Some(v) = is_trial {
        q = q.bind(v as i64);
    }
    if let Some(opt_p) = price_sats_override {
        q = q.bind(opt_p);
    }
    let ent_json;
    if let Some(ents) = entitlements {
        ent_json = serde_json::to_string(ents).unwrap_or_else(|_| "[]".into());
        q = q.bind(&ent_json);
    }
    let meta_json;
    if let Some(m) = metadata {
        meta_json = serde_json::to_string(m).unwrap_or_else(|_| "{}".into());
        q = q.bind(&meta_json);
    }
    q = q.bind(&now).bind(id);
    let rows = q.execute(pool).await?.rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("policy {id}")));
    }
    get_policy_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("policy {id}")))
}

pub async fn set_policy_public(pool: &SqlitePool, id: &str, public: bool) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query("UPDATE policies SET public = ?, updated_at = ? WHERE id = ?")
        .bind(public as i64)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("policy {id}")));
    }
    Ok(())
}

pub async fn set_policy_active(pool: &SqlitePool, id: &str, active: bool) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query("UPDATE policies SET active = ?, updated_at = ? WHERE id = ?")
        .bind(active as i64)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("policy {id}")));
    }
    Ok(())
}

fn row_to_policy(row: sqlx::sqlite::SqliteRow) -> Policy {
    let entitlements_json: String = row.get("entitlements_json");
    let entitlements: Vec<String> =
        serde_json::from_str(&entitlements_json).unwrap_or_default();
    let metadata_json: String = row.get("metadata_json");
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap_or_default();
    let active_int: i64 = row.get("active");
    let is_trial_int: i64 = row.get("is_trial");
    let public_int: i64 = row.try_get("public").unwrap_or(1);
    Policy {
        id: row.get("id"),
        product_id: row.get("product_id"),
        name: row.get("name"),
        slug: row.get("slug"),
        duration_seconds: row.get("duration_seconds"),
        grace_seconds: row.get("grace_seconds"),
        max_machines: row.get("max_machines"),
        is_trial: is_trial_int != 0,
        price_sats_override: row.get("price_sats_override"),
        entitlements,
        metadata,
        active: active_int != 0,
        public: public_int != 0,
        tip_recipient: row.get("tip_recipient"),
        tip_pct_bps: row.get("tip_pct_bps"),
        tip_label: row.get("tip_label"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// Update the tip-recipient configuration on a policy. Pass `recipient = None`
/// (and any pct/label) to disable tipping. Caller is responsible for input
/// validation; we cap pct at 10000bps here as a defense-in-depth.
pub async fn set_policy_tip_config(
    pool: &SqlitePool,
    id: &str,
    recipient: Option<&str>,
    pct_bps: i64,
    label: Option<&str>,
) -> AppResult<Policy> {
    let pct = pct_bps.clamp(0, 10_000);
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE policies SET tip_recipient = ?, tip_pct_bps = ?, tip_label = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(recipient)
    .bind(pct)
    .bind(label)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("policy {id}")));
    }
    get_policy_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("updated policy not found")))
}

// ---------- Tip attempts (audit log) ----------

#[derive(Debug, Clone, serde::Serialize)]
pub struct TipAttempt {
    pub id: String,
    pub license_id: String,
    pub policy_id: String,
    pub recipient: String,
    pub amount_sats: i64,
    pub pct_bps: i64,
    pub label: Option<String>,
    pub status: String,
    pub detail: Option<String>,
    pub payment_hash: Option<String>,
    pub created_at: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn record_tip_attempt(
    pool: &SqlitePool,
    license_id: &str,
    policy_id: &str,
    recipient: &str,
    amount_sats: i64,
    pct_bps: i64,
    label: Option<&str>,
    status: &str,
    detail: Option<&str>,
    payment_hash: Option<&str>,
) -> AppResult<()> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO tip_attempts
            (id, license_id, policy_id, recipient, amount_sats, pct_bps, label,
             status, detail, payment_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(license_id)
    .bind(policy_id)
    .bind(recipient)
    .bind(amount_sats)
    .bind(pct_bps)
    .bind(label)
    .bind(status)
    .bind(detail)
    .bind(payment_hash)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_tip_attempts(
    pool: &SqlitePool,
    license_id: Option<&str>,
    recipient: Option<&str>,
    limit: i64,
) -> AppResult<Vec<TipAttempt>> {
    let mut sql = String::from(
        "SELECT id, license_id, policy_id, recipient, amount_sats, pct_bps,
                label, status, detail, payment_hash, created_at
         FROM tip_attempts WHERE 1=1",
    );
    if license_id.is_some() {
        sql.push_str(" AND license_id = ?");
    }
    if recipient.is_some() {
        sql.push_str(" AND recipient = ?");
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");

    let mut q = sqlx::query(&sql);
    if let Some(lid) = license_id {
        q = q.bind(lid);
    }
    if let Some(r) = recipient {
        q = q.bind(r);
    }
    q = q.bind(limit.max(1).min(1000));
    let rows = q.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| TipAttempt {
            id: r.get("id"),
            license_id: r.get("license_id"),
            policy_id: r.get("policy_id"),
            recipient: r.get("recipient"),
            amount_sats: r.get("amount_sats"),
            pct_bps: r.get("pct_bps"),
            label: r.get("label"),
            status: r.get("status"),
            detail: r.get("detail"),
            payment_hash: r.get("payment_hash"),
            created_at: r.get("created_at"),
        })
        .collect())
}

// ---------- Machines ----------

const MACHINE_COLS: &str = "id, license_id, fingerprint, fingerprint_hash, hostname, platform,
                            ip_last_seen, activated_at, last_heartbeat_at,
                            deactivated_at, deactivation_reason";

pub async fn list_active_machines(
    pool: &SqlitePool,
    license_id: &str,
) -> AppResult<Vec<Machine>> {
    let rows = sqlx::query(&format!(
        "SELECT {MACHINE_COLS} FROM machines
         WHERE license_id = ? AND deactivated_at IS NULL
         ORDER BY activated_at ASC"
    ))
    .bind(license_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_machine).collect())
}

pub async fn list_all_machines(pool: &SqlitePool, license_id: &str) -> AppResult<Vec<Machine>> {
    let rows = sqlx::query(&format!(
        "SELECT {MACHINE_COLS} FROM machines WHERE license_id = ? ORDER BY activated_at DESC"
    ))
    .bind(license_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_machine).collect())
}

pub async fn get_active_machine_by_fp(
    pool: &SqlitePool,
    license_id: &str,
    fingerprint_hash: &str,
) -> AppResult<Option<Machine>> {
    let row = sqlx::query(&format!(
        "SELECT {MACHINE_COLS} FROM machines
         WHERE license_id = ? AND fingerprint_hash = ? AND deactivated_at IS NULL
         LIMIT 1"
    ))
    .bind(license_id)
    .bind(fingerprint_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_machine))
}

pub async fn get_machine_by_id(pool: &SqlitePool, id: &str) -> AppResult<Option<Machine>> {
    let row = sqlx::query(&format!("SELECT {MACHINE_COLS} FROM machines WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_machine))
}

#[allow(clippy::too_many_arguments)]
pub async fn activate_machine(
    pool: &SqlitePool,
    license_id: &str,
    fingerprint: &str,
    fingerprint_hash: &str,
    hostname: Option<&str>,
    platform: Option<&str>,
    ip_last_seen: Option<&str>,
) -> AppResult<Machine> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO machines
           (id, license_id, fingerprint, fingerprint_hash, hostname, platform,
            ip_last_seen, activated_at, last_heartbeat_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(license_id)
    .bind(fingerprint)
    .bind(fingerprint_hash)
    .bind(hostname)
    .bind(platform)
    .bind(ip_last_seen)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_machine_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created machine not found")))
}

pub async fn heartbeat_machine(
    pool: &SqlitePool,
    machine_id: &str,
    ip: Option<&str>,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE machines SET last_heartbeat_at = ?, ip_last_seen = COALESCE(?, ip_last_seen)
         WHERE id = ? AND deactivated_at IS NULL",
    )
    .bind(&now)
    .bind(ip)
    .bind(machine_id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("active machine {machine_id}")));
    }
    Ok(())
}

pub async fn deactivate_machine(
    pool: &SqlitePool,
    machine_id: &str,
    reason: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE machines SET deactivated_at = ?, deactivation_reason = ?
         WHERE id = ? AND deactivated_at IS NULL",
    )
    .bind(&now)
    .bind(reason)
    .bind(machine_id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("active machine {machine_id}")));
    }
    Ok(())
}

fn row_to_machine(row: sqlx::sqlite::SqliteRow) -> Machine {
    Machine {
        id: row.get("id"),
        license_id: row.get("license_id"),
        fingerprint: row.get("fingerprint"),
        fingerprint_hash: row.get("fingerprint_hash"),
        hostname: row.get("hostname"),
        platform: row.get("platform"),
        ip_last_seen: row.get("ip_last_seen"),
        activated_at: row.get("activated_at"),
        last_heartbeat_at: row.get("last_heartbeat_at"),
        deactivated_at: row.get("deactivated_at"),
        deactivation_reason: row.get("deactivation_reason"),
    }
}

// ---------- Webhook endpoints ----------

const WEBHOOK_COLS: &str =
    "id, url, secret, event_types, active, description, created_at, updated_at";

pub async fn create_webhook_endpoint(
    pool: &SqlitePool,
    url: &str,
    secret: &str,
    event_types: &[String],
    description: &str,
) -> AppResult<WebhookEndpoint> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let event_types_json = serde_json::to_string(event_types).unwrap_or_else(|_| "[\"*\"]".into());
    sqlx::query(
        "INSERT INTO webhook_endpoints
           (id, url, secret, event_types, active, description, created_at, updated_at)
         VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(&id)
    .bind(url)
    .bind(secret)
    .bind(&event_types_json)
    .bind(description)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_webhook_endpoint_by_id(pool, &id, /* include_secret */ true)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created webhook endpoint not found")))
}

pub async fn get_webhook_endpoint_by_id(
    pool: &SqlitePool,
    id: &str,
    include_secret: bool,
) -> AppResult<Option<WebhookEndpoint>> {
    let row = sqlx::query(&format!(
        "SELECT {WEBHOOK_COLS} FROM webhook_endpoints WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| row_to_webhook_endpoint(r, include_secret)))
}

pub async fn list_webhook_endpoints(
    pool: &SqlitePool,
    include_secret: bool,
) -> AppResult<Vec<WebhookEndpoint>> {
    let rows = sqlx::query(&format!(
        "SELECT {WEBHOOK_COLS} FROM webhook_endpoints ORDER BY created_at DESC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| row_to_webhook_endpoint(r, include_secret))
        .collect())
}

pub async fn list_active_webhook_endpoints(
    pool: &SqlitePool,
) -> AppResult<Vec<WebhookEndpoint>> {
    let rows = sqlx::query(&format!(
        "SELECT {WEBHOOK_COLS} FROM webhook_endpoints WHERE active = 1"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| row_to_webhook_endpoint(r, true)).collect())
}

pub async fn set_webhook_active(pool: &SqlitePool, id: &str, active: bool) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE webhook_endpoints SET active = ?, updated_at = ? WHERE id = ?",
    )
    .bind(active as i64)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("webhook endpoint {id}")));
    }
    Ok(())
}

pub async fn delete_webhook_endpoint(pool: &SqlitePool, id: &str) -> AppResult<()> {
    let rows = sqlx::query("DELETE FROM webhook_endpoints WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(AppError::NotFound(format!("webhook endpoint {id}")));
    }
    Ok(())
}

fn row_to_webhook_endpoint(row: sqlx::sqlite::SqliteRow, include_secret: bool) -> WebhookEndpoint {
    let event_types_json: String = row.get("event_types");
    let event_types: Vec<String> =
        serde_json::from_str(&event_types_json).unwrap_or_else(|_| vec!["*".to_string()]);
    let active_int: i64 = row.get("active");
    let secret = if include_secret {
        Some(row.get::<String, _>("secret"))
    } else {
        None
    };
    WebhookEndpoint {
        id: row.get("id"),
        url: row.get("url"),
        secret,
        event_types,
        active: active_int != 0,
        description: row.get("description"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

// ---------- Webhook deliveries ----------

pub async fn enqueue_delivery(
    pool: &SqlitePool,
    endpoint_id: &str,
    event_type: &str,
    payload_json: &str,
) -> AppResult<WebhookDelivery> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO webhook_deliveries
           (id, endpoint_id, event_type, payload_json, attempt_count,
            next_attempt_at, created_at)
         VALUES (?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(endpoint_id)
    .bind(event_type)
    .bind(payload_json)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let row = sqlx::query(
        "SELECT id, endpoint_id, event_type, payload_json, attempt_count,
                next_attempt_at, last_status_code, last_error, delivered_at, created_at
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(pool)
    .await?;
    Ok(row_to_delivery(row))
}

pub async fn list_ready_deliveries(
    pool: &SqlitePool,
    limit: i64,
) -> AppResult<Vec<WebhookDelivery>> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "SELECT id, endpoint_id, event_type, payload_json, attempt_count,
                next_attempt_at, last_status_code, last_error, delivered_at, created_at
         FROM webhook_deliveries
         WHERE delivered_at IS NULL AND next_attempt_at IS NOT NULL AND next_attempt_at <= ?
         ORDER BY next_attempt_at ASC LIMIT ?",
    )
    .bind(&now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_delivery).collect())
}

pub async fn mark_delivery_success(
    pool: &SqlitePool,
    id: &str,
    status_code: i64,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE webhook_deliveries
         SET delivered_at = ?, next_attempt_at = NULL, last_status_code = ?, attempt_count = attempt_count + 1
         WHERE id = ?",
    )
    .bind(&now)
    .bind(status_code)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_delivery_failure(
    pool: &SqlitePool,
    id: &str,
    status_code: Option<i64>,
    error: &str,
    next_attempt_at: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE webhook_deliveries
         SET attempt_count = attempt_count + 1,
             last_status_code = ?,
             last_error = ?,
             next_attempt_at = ?
         WHERE id = ?",
    )
    .bind(status_code)
    .bind(error)
    .bind(next_attempt_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Filter modes for `list_deliveries`. Strings match the values
/// accepted by the admin endpoint's `?status=...` query param.
pub enum DeliveryStatusFilter {
    /// `delivered_at IS NULL AND next_attempt_at IS NOT NULL` — in
    /// the retry queue, will be picked up by the worker on the next
    /// tick that's past `next_attempt_at`.
    Pending,
    /// `delivered_at IS NOT NULL` — successfully delivered.
    Delivered,
    /// `delivered_at IS NULL AND next_attempt_at IS NULL AND
    /// attempt_count > 0` — the dead-letter case. Worker exhausted
    /// retries (or hit a hard error like a deleted endpoint) and
    /// won't re-pick it. Operators see these via the admin list and
    /// can manually re-queue via `requeue_delivery`.
    Failed,
    /// All deliveries.
    All,
}

impl DeliveryStatusFilter {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// List webhook deliveries with optional filtering. Newest first
/// (orders by `created_at DESC`) so the admin UI shows recent
/// activity at the top.
pub async fn list_deliveries(
    pool: &SqlitePool,
    endpoint_id: Option<&str>,
    status: DeliveryStatusFilter,
    limit: i64,
) -> AppResult<Vec<WebhookDelivery>> {
    let mut sql = String::from(
        "SELECT id, endpoint_id, event_type, payload_json, attempt_count,
                next_attempt_at, last_status_code, last_error, delivered_at, created_at
         FROM webhook_deliveries WHERE 1=1",
    );
    if endpoint_id.is_some() {
        sql.push_str(" AND endpoint_id = ?");
    }
    match status {
        DeliveryStatusFilter::Pending => {
            sql.push_str(" AND delivered_at IS NULL AND next_attempt_at IS NOT NULL")
        }
        DeliveryStatusFilter::Delivered => sql.push_str(" AND delivered_at IS NOT NULL"),
        DeliveryStatusFilter::Failed => sql.push_str(
            " AND delivered_at IS NULL AND next_attempt_at IS NULL AND attempt_count > 0",
        ),
        DeliveryStatusFilter::All => {}
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");

    let mut q = sqlx::query(&sql);
    if let Some(eid) = endpoint_id {
        q = q.bind(eid);
    }
    q = q.bind(limit);
    let rows = q.fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_delivery).collect())
}

/// Re-queue a previously-failed (or even successfully-delivered)
/// delivery for another attempt. Resets `attempt_count` to 0, clears
/// `delivered_at` and `last_error`, and sets `next_attempt_at` to
/// now so the worker picks it up on the next tick.
///
/// Returns the affected row, or `Ok(None)` if no row with the given
/// id exists.
pub async fn requeue_delivery(
    pool: &SqlitePool,
    id: &str,
) -> AppResult<Option<WebhookDelivery>> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE webhook_deliveries
         SET attempt_count = 0,
             delivered_at = NULL,
             last_error = NULL,
             last_status_code = NULL,
             next_attempt_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT id, endpoint_id, event_type, payload_json, attempt_count,
                next_attempt_at, last_status_code, last_error, delivered_at, created_at
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(Some(row_to_delivery(row)))
}

fn row_to_delivery(row: sqlx::sqlite::SqliteRow) -> WebhookDelivery {
    WebhookDelivery {
        id: row.get("id"),
        endpoint_id: row.get("endpoint_id"),
        event_type: row.get("event_type"),
        payload_json: row.get("payload_json"),
        attempt_count: row.get("attempt_count"),
        next_attempt_at: row.get("next_attempt_at"),
        last_status_code: row.get("last_status_code"),
        last_error: row.get("last_error"),
        delivered_at: row.get("delivered_at"),
        created_at: row.get("created_at"),
    }
}

// ---------- Audit log ----------

#[allow(clippy::too_many_arguments)]
pub async fn insert_audit(
    pool: &SqlitePool,
    actor_kind: &str,
    actor_hash: Option<&str>,
    action: &str,
    target_kind: Option<&str>,
    target_id: Option<&str>,
    request_ip: Option<&str>,
    user_agent: Option<&str>,
    details: &serde_json::Value,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let details_json = serde_json::to_string(details).unwrap_or_else(|_| "{}".into());
    sqlx::query(
        "INSERT INTO audit_log
           (actor_kind, actor_hash, action, target_kind, target_id, request_ip, user_agent,
            details_json, occurred_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(actor_kind)
    .bind(actor_hash)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .bind(request_ip)
    .bind(user_agent)
    .bind(&details_json)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_audit(
    pool: &SqlitePool,
    limit: i64,
    action_filter: Option<&str>,
) -> AppResult<Vec<AuditEntry>> {
    let rows = match action_filter {
        Some(a) => {
            sqlx::query(
                "SELECT id, actor_kind, actor_hash, action, target_kind, target_id,
                        request_ip, user_agent, details_json, occurred_at
                 FROM audit_log WHERE action = ? ORDER BY id DESC LIMIT ?",
            )
            .bind(a)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, actor_kind, actor_hash, action, target_kind, target_id,
                        request_ip, user_agent, details_json, occurred_at
                 FROM audit_log ORDER BY id DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            let details_json: String = row.get("details_json");
            let details: serde_json::Value =
                serde_json::from_str(&details_json).unwrap_or_default();
            AuditEntry {
                id: row.get("id"),
                actor_kind: row.get("actor_kind"),
                actor_hash: row.get("actor_hash"),
                action: row.get("action"),
                target_kind: row.get("target_kind"),
                target_id: row.get("target_id"),
                request_ip: row.get("request_ip"),
                user_agent: row.get("user_agent"),
                details,
                occurred_at: row.get("occurred_at"),
            }
        })
        .collect())
}

// ---------- Discount codes ----------

fn row_to_discount_code(row: sqlx::sqlite::SqliteRow) -> DiscountCode {
    DiscountCode {
        id: row.get("id"),
        code: row.get("code"),
        kind: row.get("kind"),
        amount: row.get("amount"),
        max_uses: row.get("max_uses"),
        used_count: row.get("used_count"),
        expires_at: row.get("expires_at"),
        applies_to_product_id: row.get("applies_to_product_id"),
        applies_to_policy_id: row.get("applies_to_policy_id"),
        referrer_label: row.get("referrer_label"),
        description: row.get("description"),
        active: row.get::<i64, _>("active") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_discount_redemption(row: sqlx::sqlite::SqliteRow) -> DiscountRedemption {
    DiscountRedemption {
        id: row.get("id"),
        code_id: row.get("code_id"),
        invoice_id: row.get("invoice_id"),
        license_id: row.get("license_id"),
        status: row.get("status"),
        discount_applied_sats: row.get("discount_applied_sats"),
        base_price_sats: row.get("base_price_sats"),
        final_price_sats: row.get("final_price_sats"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create_discount_code(
    pool: &SqlitePool,
    code: &str,
    kind: &str,
    amount: i64,
    max_uses: Option<i64>,
    expires_at: Option<&str>,
    applies_to_product_id: Option<&str>,
    applies_to_policy_id: Option<&str>,
    referrer_label: Option<&str>,
    description: &str,
) -> AppResult<DiscountCode> {
    if !matches!(
        kind,
        "percent" | "fixed_sats" | "set_price" | "free_license"
    ) {
        return Err(AppError::BadRequest(format!(
            "discount kind must be 'percent', 'fixed_sats', 'set_price', or 'free_license', got '{kind}'"
        )));
    }
    if amount < 0 {
        return Err(AppError::BadRequest("amount must be >= 0".into()));
    }
    if kind == "percent" && amount > 10_000 {
        return Err(AppError::BadRequest(
            "percent amount must be in basis points (0..=10000); 10000 = 100%".into(),
        ));
    }
    if kind == "fixed_sats" && amount == 0 {
        return Err(AppError::BadRequest(
            "fixed_sats amount must be > 0".into(),
        ));
    }
    if kind == "set_price" && amount <= 0 {
        return Err(AppError::BadRequest(
            "set_price amount (the buyer's flat-price target, in sats) must be > 0".into(),
        ));
    }
    // free_license codes ignore `amount`; we force it to 0 on insert below.
    if let Some(m) = max_uses {
        if m <= 0 {
            return Err(AppError::BadRequest(
                "max_uses must be > 0 (or omitted for unlimited)".into(),
            ));
        }
    }
    let normalized = code.trim().to_uppercase();
    if normalized.is_empty() {
        return Err(AppError::BadRequest("code must not be empty".into()));
    }
    if !normalized
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::BadRequest(
            "code must use ASCII alphanumerics, '-', or '_'".into(),
        ));
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let stored_amount = if kind == "free_license" { 0 } else { amount };
    sqlx::query(
        "INSERT INTO discount_codes
         (id, code, kind, amount, max_uses, used_count, expires_at,
          applies_to_product_id, applies_to_policy_id, referrer_label,
          description, active, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(&normalized)
    .bind(kind)
    .bind(stored_amount)
    .bind(max_uses)
    .bind(expires_at)
    .bind(applies_to_product_id)
    .bind(applies_to_policy_id)
    .bind(referrer_label)
    .bind(description)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("discount code '{normalized}' already exists"))
        }
        other => AppError::Database(other),
    })?;
    get_discount_code_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created discount code not found")))
}

pub async fn get_discount_code_by_id(
    pool: &SqlitePool,
    id: &str,
) -> AppResult<Option<DiscountCode>> {
    let row = sqlx::query(
        "SELECT id, code, kind, amount, max_uses, used_count, expires_at,
                applies_to_product_id, applies_to_policy_id, referrer_label,
                description, active, created_at, updated_at
         FROM discount_codes WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_discount_code))
}

pub async fn get_discount_code_by_code(
    pool: &SqlitePool,
    code: &str,
) -> AppResult<Option<DiscountCode>> {
    let normalized = code.trim().to_uppercase();
    let row = sqlx::query(
        "SELECT id, code, kind, amount, max_uses, used_count, expires_at,
                applies_to_product_id, applies_to_policy_id, referrer_label,
                description, active, created_at, updated_at
         FROM discount_codes WHERE code = ?",
    )
    .bind(&normalized)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_discount_code))
}

pub async fn list_discount_codes(
    pool: &SqlitePool,
    only_active: bool,
) -> AppResult<Vec<DiscountCode>> {
    let q = if only_active {
        "SELECT id, code, kind, amount, max_uses, used_count, expires_at,
                applies_to_product_id, applies_to_policy_id, referrer_label,
                description, active, created_at, updated_at
         FROM discount_codes WHERE active = 1 ORDER BY created_at DESC"
    } else {
        "SELECT id, code, kind, amount, max_uses, used_count, expires_at,
                applies_to_product_id, applies_to_policy_id, referrer_label,
                description, active, created_at, updated_at
         FROM discount_codes ORDER BY created_at DESC"
    };
    let rows = sqlx::query(q).fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_discount_code).collect())
}

pub async fn set_discount_code_active(
    pool: &SqlitePool,
    id: &str,
    active: bool,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE discount_codes SET active = ?, updated_at = ? WHERE id = ?")
        .bind(active as i64)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Patch mutable fields on a discount code. Mutable fields are the ones
/// that don't change behavior in confusing ways for codes already in
/// circulation: `amount`, `max_uses`, `expires_at`, `description`,
/// `referrer_label`. The code string itself, kind, and product/policy
/// scope are intentionally NOT editable — changing those would silently
/// invalidate links that are already out in the wild. Operators should
/// disable + create a new code instead. Each `Option<T>` parameter is
/// `Some(value_or_clear)` to update, `None` to leave alone; for fields
/// that can be NULL'd, callers pass `Some(None)` to clear.
#[allow(clippy::too_many_arguments)]
pub async fn update_discount_code(
    pool: &SqlitePool,
    id: &str,
    amount: Option<i64>,
    max_uses: Option<Option<i64>>,
    expires_at: Option<Option<&str>>,
    description: Option<&str>,
    referrer_label: Option<Option<&str>>,
) -> AppResult<DiscountCode> {
    // Re-fetch to validate amount against the existing kind.
    let existing = get_discount_code_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("discount code {id}")))?;
    if let Some(a) = amount {
        if a < 0 {
            return Err(AppError::BadRequest("amount must be >= 0".into()));
        }
        if existing.kind == "percent" && a > 10_000 {
            return Err(AppError::BadRequest(
                "percent amount must be in basis points (0..=10000); 10000 = 100%".into(),
            ));
        }
        if existing.kind == "fixed_sats" && a == 0 {
            return Err(AppError::BadRequest(
                "fixed_sats amount must be > 0".into(),
            ));
        }
        if existing.kind == "set_price" && a <= 0 {
            return Err(AppError::BadRequest(
                "set_price amount (the buyer's flat-price target, in sats) must be > 0".into(),
            ));
        }
        if existing.kind == "free_license" && a != 0 {
            return Err(AppError::BadRequest(
                "free_license codes have no amount; pass 0 or leave unchanged".into(),
            ));
        }
    }
    if let Some(Some(m)) = max_uses {
        if m <= 0 {
            return Err(AppError::BadRequest(
                "max_uses must be > 0 (or pass null to clear it for unlimited)".into(),
            ));
        }
        if m < existing.used_count {
            return Err(AppError::BadRequest(format!(
                "max_uses ({m}) cannot be lower than the current used_count ({})",
                existing.used_count
            )));
        }
    }

    let mut sets: Vec<&str> = Vec::new();
    if amount.is_some() {
        sets.push("amount = ?");
    }
    if max_uses.is_some() {
        sets.push("max_uses = ?");
    }
    if expires_at.is_some() {
        sets.push("expires_at = ?");
    }
    if description.is_some() {
        sets.push("description = ?");
    }
    if referrer_label.is_some() {
        sets.push("referrer_label = ?");
    }
    if sets.is_empty() {
        return Ok(existing);
    }
    sets.push("updated_at = ?");
    let sql = format!(
        "UPDATE discount_codes SET {} WHERE id = ?",
        sets.join(", ")
    );
    let now = Utc::now().to_rfc3339();
    let mut q = sqlx::query(&sql);
    if let Some(a) = amount {
        q = q.bind(a);
    }
    if let Some(opt_m) = max_uses {
        q = q.bind(opt_m);
    }
    if let Some(opt_e) = expires_at {
        q = q.bind(opt_e);
    }
    if let Some(d) = description {
        q = q.bind(d);
    }
    if let Some(opt_r) = referrer_label {
        q = q.bind(opt_r);
    }
    q = q.bind(&now).bind(id);
    q.execute(pool).await?;

    get_discount_code_by_id(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("discount code {id}")))
}

/// Phase 1 of redemption: atomically increment `used_count` on the
/// discount code, gated on active/not-expired/has-uses-remaining. Returns
/// `BadRequest` if any of those checks fails. The caller MUST follow up
/// with either `record_pending_redemption` (on success path) or
/// `release_code_slot` (if the BTCPay invoice fails to create).
pub async fn try_reserve_code_slot(pool: &SqlitePool, code_id: &str) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE discount_codes
         SET used_count = used_count + 1, updated_at = ?
         WHERE id = ?
           AND active = 1
           AND (expires_at IS NULL OR expires_at > ?)
           AND (max_uses IS NULL OR used_count < max_uses)",
    )
    .bind(&now)
    .bind(code_id)
    .bind(&now)
    .execute(pool)
    .await?;
    if res.rows_affected() != 1 {
        return Err(AppError::BadRequest(
            "discount code is invalid, expired, or has no remaining uses".into(),
        ));
    }
    Ok(())
}

/// Inverse of `try_reserve_code_slot` — decrements the counter so the
/// freed slot becomes available again. Use this if BTCPay invoice
/// creation fails AFTER the slot was reserved. Saturates at zero so a
/// double-release can't drive the counter negative.
pub async fn release_code_slot(pool: &SqlitePool, code_id: &str) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE discount_codes
         SET used_count = MAX(used_count - 1, 0), updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(code_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Phase 2 of redemption: persist the pending row that ties the reserved
/// slot to a specific invoice. Call after `try_reserve_code_slot` and the
/// BTCPay invoice + local invoice rows are in place.
#[allow(clippy::too_many_arguments)]
pub async fn record_pending_redemption(
    pool: &SqlitePool,
    code_id: &str,
    invoice_id: &str,
    discount_applied_sats: i64,
    base_price_sats: i64,
    final_price_sats: i64,
) -> AppResult<DiscountRedemption> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO discount_redemptions
         (id, code_id, invoice_id, license_id, status,
          discount_applied_sats, base_price_sats, final_price_sats,
          created_at, updated_at)
         VALUES (?, ?, ?, NULL, 'pending', ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(code_id)
    .bind(invoice_id)
    .bind(discount_applied_sats)
    .bind(base_price_sats)
    .bind(final_price_sats)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    get_discount_redemption_by_id(pool, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("inserted redemption not found")))
}

pub async fn get_discount_redemption_by_id(
    pool: &SqlitePool,
    id: &str,
) -> AppResult<Option<DiscountRedemption>> {
    let row = sqlx::query(
        "SELECT id, code_id, invoice_id, license_id, status,
                discount_applied_sats, base_price_sats, final_price_sats,
                created_at, updated_at
         FROM discount_redemptions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_discount_redemption))
}

pub async fn get_pending_redemption_by_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
) -> AppResult<Option<DiscountRedemption>> {
    let row = sqlx::query(
        "SELECT id, code_id, invoice_id, license_id, status,
                discount_applied_sats, base_price_sats, final_price_sats,
                created_at, updated_at
         FROM discount_redemptions WHERE invoice_id = ? AND status = 'pending'",
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_discount_redemption))
}

pub async fn list_redemptions_by_code(
    pool: &SqlitePool,
    code_id: &str,
) -> AppResult<Vec<DiscountRedemption>> {
    let rows = sqlx::query(
        "SELECT id, code_id, invoice_id, license_id, status,
                discount_applied_sats, base_price_sats, final_price_sats,
                created_at, updated_at
         FROM discount_redemptions WHERE code_id = ? ORDER BY created_at DESC",
    )
    .bind(code_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_discount_redemption).collect())
}

/// Mark a pending redemption as redeemed and attach the issued license id.
/// Idempotent: if the redemption is already redeemed we return without
/// changing anything.
pub async fn mark_redemption_redeemed(
    pool: &SqlitePool,
    redemption_id: &str,
    license_id: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE discount_redemptions
         SET status = 'redeemed', license_id = ?, updated_at = ?
         WHERE id = ? AND status = 'pending'",
    )
    .bind(license_id)
    .bind(&now)
    .bind(redemption_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a pending redemption as cancelled and decrement `used_count` so
/// the freed slot becomes available again.
pub async fn cancel_redemption(
    pool: &SqlitePool,
    redemption_id: &str,
) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT code_id, status FROM discount_redemptions WHERE id = ?")
        .bind(redemption_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(());
    };
    let status: String = row.get("status");
    if status != "pending" {
        tx.rollback().await?;
        return Ok(());
    }
    let code_id: String = row.get("code_id");
    sqlx::query(
        "UPDATE discount_redemptions
         SET status = 'cancelled', updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(redemption_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE discount_codes
         SET used_count = MAX(used_count - 1, 0), updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&code_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------- Settings (runtime key-value store) ----------

/// Read a value from the runtime settings table. Returns Ok(None) if
/// the key has never been set or has been explicitly cleared.
pub async fn settings_get(pool: &SqlitePool, key: &str) -> AppResult<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>("value")))
}

// ---------- Web UI sessions ----------

/// Create a new session row. Token is the random URL-safe base64 string
/// (callers generate it with `crate::api::auth::new_session_token`).
pub async fn create_session(
    pool: &SqlitePool,
    token: &str,
    created_at: &str,
    expires_at: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO sessions (token, created_at, expires_at, last_seen_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(token)
    .bind(created_at)
    .bind(expires_at)
    .bind(created_at) // last_seen_at = created_at on insert
    .bind(ip)
    .bind(user_agent)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns true if the session exists and hasn't expired. Side-effect:
/// bumps `last_seen_at` so an active session stays alive (sliding window).
pub async fn is_session_valid(pool: &SqlitePool, token: &str) -> AppResult<bool> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT token, expires_at FROM sessions WHERE token = ?",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    let Some((_, expires_at)) = row else { return Ok(false) };
    let exp = match chrono::DateTime::parse_from_rfc3339(&expires_at) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => return Ok(false),
    };
    if exp < Utc::now() {
        return Ok(false);
    }
    let now = Utc::now().to_rfc3339();
    let _ = sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token = ?")
        .bind(&now)
        .bind(token)
        .execute(pool)
        .await;
    Ok(true)
}

/// Hard-delete a single session row. Idempotent.
pub async fn delete_session(pool: &SqlitePool, token: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

/// Wipe every session row — used on password rotation.
pub async fn delete_all_sessions(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions").execute(pool).await?;
    Ok(())
}

/// Background cleanup: drop sessions whose `expires_at` is in the past.
/// Returns the number of rows removed (for logging).
pub async fn reap_expired_sessions(pool: &SqlitePool) -> AppResult<u64> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Upsert a key into the runtime settings table. Pass `None` to clear it.
pub async fn settings_set(pool: &SqlitePool, key: &str, value: Option<&str>) -> AppResult<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}
