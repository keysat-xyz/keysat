//! Embedded admin web UI.
//!
//! At compile time, every file in `licensing-service/web/` is bundled
//! into the binary via `rust-embed`. At runtime, axum serves them under
//! `/admin/*` — no separate static-file deployment, no nginx, no proxy.
//! The whole admin SPA ships in the same `keysat` executable as the
//! daemon.
//!
//! Auth model: NONE at this HTTP layer. The static assets themselves
//! (HTML, CSS, JS) are public — there's nothing secret in them. The
//! actual gating happens client-side: the index page prompts for the
//! operator's admin API key on first load, validates it against any
//! `/v1/admin/*` endpoint, stores it in localStorage, and uses it as
//! `Authorization: Bearer ...` on every subsequent admin call. The
//! admin-scoped endpoints already enforce the key constant-time, so a
//! random visitor can load `/admin/index.html` but cannot do anything
//! useful without the key.
//!
//! v0.2 first cut: this is scaffolding only. The HTML page contains a
//! login form + a placeholder dashboard. Future SPA work just adds
//! more files into `web/` (or replaces index.html with a built React /
//! Svelte bundle); the serving code below doesn't change.

use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
};
use rust_embed::RustEmbed;

/// Compile-time-bundled directory of static admin UI assets. Every file
/// under `web/` (relative to the crate root) is embedded byte-for-byte
/// into the binary.
#[derive(RustEmbed)]
#[folder = "web/"]
struct AdminAssets;

/// `GET /admin` — redirect to `/admin/` so the relative paths in the
/// embedded HTML resolve correctly.
pub async fn admin_root_redirect() -> Redirect {
    Redirect::permanent("/admin/")
}

/// `GET /admin/` — serve the SPA shell (index.html).
pub async fn admin_index() -> Response {
    serve_embedded("index.html")
}

/// `GET /admin/*path` — serve any other embedded static file. Falls
/// through to `index.html` for unknown paths so client-side routing
/// (e.g. /admin/products, /admin/licenses) works without server-side
/// route registration.
pub async fn admin_asset(uri: Uri) -> Response {
    // The Uri here will be the FULL path (including the /admin prefix).
    // Strip the prefix to look up the asset.
    let path = uri.path();
    let stripped = path.strip_prefix("/admin/").unwrap_or(path);
    if stripped.is_empty() {
        return serve_embedded("index.html");
    }
    if AdminAssets::get(stripped).is_some() {
        serve_embedded(stripped)
    } else {
        // Unknown path — fall through to index.html so the SPA's
        // client-side router can take over. This is the canonical
        // fallback pattern for SPAs hosted on path prefixes.
        serve_embedded("index.html")
    }
}

fn serve_embedded(path: &str) -> Response {
    match AdminAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.to_string())
                // Modest caching — these are versioned with the binary,
                // so cache for an hour. A binary upgrade rolls the
                // service which evicts the cache anyway.
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
