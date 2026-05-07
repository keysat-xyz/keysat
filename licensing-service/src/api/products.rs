//! Public product endpoints.

use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{json, Value};

pub async fn list(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let products = repo::list_products(&state.db, true).await?;
    Ok(Json(json!({ "products": products })))
}

pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<Value>> {
    let product = repo::get_product_by_slug(&state.db, &slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{slug}'")))?;
    Ok(Json(json!(product)))
}
