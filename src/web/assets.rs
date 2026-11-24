//! Embedded static assets, so the server stays a single binary.

use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, max-age=0"),
    );
    response
}

pub async fn css() -> Response {
    asset("text/css; charset=utf-8", include_str!("static/app.css"))
}

pub async fn js() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("static/app.js"),
    )
}

pub async fn favicon() -> Response {
    asset("image/svg+xml", include_str!("static/favicon.svg"))
}
