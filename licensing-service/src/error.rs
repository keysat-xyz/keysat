//! Unified error type for the service. Converts into appropriate HTTP
//! responses so handlers can just `?`-propagate.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("license invalid: {0}")]
    LicenseInvalid(String),

    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("BTCPay not configured: connect via the StartOS dashboard first")]
    BtcpayNotConfigured,

    #[error("too many requests: {0}")]
    TooManyRequests(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// 402 Payment Required — used for tier-gate enforcement when the
    /// operator's Keysat self-license doesn't include the entitlement
    /// or capacity needed for the requested operation. The fields are
    /// surfaced in the JSON body so the admin SPA can render an upgrade
    /// CTA without parsing the message string.
    #[error("payment required: {message}")]
    PaymentRequired {
        message: String,
        upgrade_url: String,
    },

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            AppError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            AppError::LicenseInvalid(_) => (StatusCode::OK, "invalid"),
            AppError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            AppError::BtcpayNotConfigured => (StatusCode::SERVICE_UNAVAILABLE, "btcpay_not_configured"),
            AppError::TooManyRequests(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            AppError::ServiceUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"),
            AppError::PaymentRequired { .. } => (StatusCode::PAYMENT_REQUIRED, "tier_cap"),
            AppError::Database(_) | AppError::Internal(_) => {
                tracing::error!(error = %self, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };

        // Tier-cap 402 carries a structured upgrade_url alongside the
        // message so the SPA can render an upgrade-CTA button without
        // having to parse a URL out of the human-facing message.
        let body = match &self {
            AppError::PaymentRequired { message, upgrade_url } => Json(json!({
                "ok": false,
                "error": code,
                "message": message,
                "upgrade_url": upgrade_url,
            })),
            _ => Json(json!({
                "ok": false,
                "error": code,
                "message": self.to_string(),
            })),
        };

        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
