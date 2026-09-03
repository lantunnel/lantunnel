//! HTTP echo service.
//!
//! Two routes:
//!
//! - `ANY /`     — echoes the request body back, prefixed with a one-line
//!   header `#<seq> size=<n>\n` where `<seq>` is the post-increment HTTP
//!   request counter and `<n>` is the request body length in bytes.
//! - `GET /stats` — text-format dump of the shared counters, one
//!   `key=value` line per counter.
//!
//! Body size is capped at 16 MiB via `DefaultBodyLimit` so a runaway client
//! cannot OOM the fixture during fuzz/chaos tests.

use std::net::SocketAddr;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

use crate::counters::Counters;

/// 16 MiB request-body cap.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Build the router. Public so smoke tests can mount it under their own
/// `TcpListener` without going through the full `serve` flow.
pub fn router(counters: Counters) -> Router {
    Router::new()
        .route("/stats", get(stats_handler))
        .route("/", any(echo_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(counters)
}

pub async fn serve(addr: SocketAddr, counters: Counters) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(addr = %bound, "http echo listening");
    axum::serve(listener, router(counters).into_make_service()).await?;
    Ok(())
}

async fn echo_handler(State(counters): State<Counters>, body: Bytes) -> Response {
    let size = body.len() as u64;
    let seq = counters.inc_http_requests();
    counters.add_http_bytes_in(size);

    // Build response body: `#<seq> size=<n>\n` + original body.
    let header_line = format!("#{seq} size={size}\n");
    let mut out = Vec::with_capacity(header_line.len() + body.len());
    out.extend_from_slice(header_line.as_bytes());
    out.extend_from_slice(&body);
    counters.add_http_bytes_out(out.len() as u64);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        out,
    )
        .into_response()
}

async fn stats_handler(State(counters): State<Counters>) -> Response {
    let body = counters.snapshot().to_text();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}
