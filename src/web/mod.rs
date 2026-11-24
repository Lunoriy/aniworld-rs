//! Web interface: an axum server, page routes and the JSON API.
//!
//! It shares the same [`crate::download::Engine`] as the CLI and TUI, so the
//! queue, naming, numbering and provider fallbacks behave identically.

pub mod api;
mod assets;
mod library;
mod pages;
pub mod state;

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::download::EVENT_BUFFER;
use crate::web::state::{now, Web};

pub async fn run(cfg: Config, host: String, port: u16) -> Result<()> {
    let (events, mut event_rx) = mpsc::channel(EVENT_BUFFER);
    let web = Web::new(cfg, env!("CARGO_PKG_VERSION"), events)?;

    {
        let web = web.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                web.apply_event(event).await;
            }
        });
    }

    {
        let web = web.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let settings = web.settings.read().await.clone();
                if !settings.autosync_enabled || web.autosync_running.load(Ordering::SeqCst) {
                    continue;
                }
                let last = *web.autosync_last_run.lock().await;
                let interval = settings.autosync_interval.max(5) * 60;
                if last == 0 || now() >= last + interval {
                    web.autosync_once().await;
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(pages::home))
        .route("/series", get(pages::series_page))
        .route("/queue", get(pages::queue_page))
        .route("/library", get(pages::library_page))
        .route("/settings", get(pages::settings_page))
        .route("/autosync", get(pages::autosync_page))
        .route("/favicon.ico", get(assets::favicon))
        .route("/static/app.css", get(assets::css))
        .route("/static/app.js", get(assets::js))
        .route("/static/favicon.svg", get(assets::favicon))
        .route("/api/health", get(api::health))
        .route("/api/status", get(api::status))
        .route("/api/search", get(api::search))
        .route("/api/series", get(api::series))
        .route("/api/season", get(api::season))
        .route("/api/download", post(api::download))
        .route("/api/queue", get(api::queue))
        .route("/api/queue/events", get(api::queue_events))
        .route("/api/queue/clear", post(api::queue_clear))
        .route("/api/queue/{id}/cancel", post(api::queue_cancel))
        .route("/api/queue/{id}/retry", post(api::queue_retry))
        .route("/api/queue/{id}/remove", post(api::queue_remove))
        .route("/api/library", get(api::library_index))
        .route("/api/library/delete", post(api::library_delete))
        .route("/media", get(api::media))
        .route("/img", get(api::image))
        .route("/api/settings", get(api::settings_get).put(api::settings_put))
        .route("/api/autosync", get(api::autosync_get).put(api::autosync_put))
        .route("/api/autosync/run", post(api::autosync_run))
        .route("/api/autosync/series", post(api::autosync_add))
        .route("/api/autosync/series/{id}", delete(api::autosync_remove))
        .route("/api/autosync/series/{id}/toggle", post(api::autosync_toggle))
        .fallback(not_found)
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(guard))
        .with_state(web.clone());

    let address = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("binding {address}"))?;
    let local = listener.local_addr()?;
    println!(
        "aniworld-rs {} web interface on http://{}",
        env!("CARGO_PKG_VERSION"),
        if local.ip().is_unspecified() {
            format!("127.0.0.1:{}", local.port())
        } else {
            local.to_string()
        }
    );
    println!("  downloads: {}", web.settings.read().await.download_path);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving web interface")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down");
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"ok": false, "error": message}))).into_response()
}

/// JSON-only mutations: a cross-site HTML form cannot set this content type,
/// and fetch() from another origin would need a CORS preflight we never allow.
async fn guard(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD && path.starts_with("/api/") {
        let json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.starts_with("application/json"))
            .unwrap_or(false);
        if !json {
            return json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            );
        }
        if let Some(site) = request
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
        {
            if !matches!(site, "same-origin" | "none") {
                return json_error(StatusCode::FORBIDDEN, "cross-site request refused");
            }
        }
    }
    next.run(request).await
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' https: data:; media-src 'self'; \
             style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; \
             base-uri 'none'; form-action 'self'; frame-ancestors 'self'",
        ),
    );
    response
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(pages::error_page(404, "That page does not exist.").0),
    )
        .into_response()
}
