//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes three counters:
//! - `txwatch_transactions_total`       — total transactions processed
//! - `txwatch_alerts_total`             — total alert payloads sent (rules matched)
//! - `txwatch_webhook_failures_total`   — total permanent webhook delivery failures
//!
//! An optional HTTP server can be started by calling [`serve_metrics`]:
//!
//! - `GET /metrics` — Prometheus text exposition format
//! - `GET /healthz` — `200` while the process is running
//! - `GET /readyz`  — `200` once a poll has succeeded within the last
//!   2 × `poll_interval_seconds`, `503` otherwise
//!
//! Any other path returns `404`, and any other method `405`.

use anyhow::{Context, Result};
use prometheus::{register_int_counter, IntCounter};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use http_body_util::Full;
use hyper::{
    body::Bytes,
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

// ── Counters ──────────────────────────────────────────────────────────────────

fn transactions_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_transactions_total",
            "Total Stellar transactions processed across all watched contracts"
        )
        .expect("register txwatch_transactions_total")
    })
}

fn alerts_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_alerts_total",
            "Total alert payloads sent (rules matched)"
        )
        .expect("register txwatch_alerts_total")
    })
}

fn webhook_failures_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_webhook_failures_total",
            "Total permanent webhook delivery failures (after all retries)"
        )
        .expect("register txwatch_webhook_failures_total")
    })
}

/// Increment the `txwatch_transactions_total` counter by `n`.
pub fn inc_transactions(n: u64) {
    transactions_total().inc_by(n);
}

/// Increment the `txwatch_alerts_total` counter by `n`.
pub fn inc_alerts(n: u64) {
    alerts_total().inc_by(n);
}

/// Increment the `txwatch_webhook_failures_total` counter by 1.
pub fn inc_webhook_failures() {
    webhook_failures_total().inc();
}

// ── Readiness ─────────────────────────────────────────────────────────────────

/// Unix seconds of the last successful contract poll (0 = never).
static LAST_POLL_SUCCESS: AtomicU64 = AtomicU64::new(0);
/// Configured poll interval in seconds, used to judge readiness.
static POLL_INTERVAL_SECS: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record the configured poll interval (called once by the poller at start-up).
pub fn set_poll_interval(secs: u64) {
    POLL_INTERVAL_SECS.store(secs, Ordering::Relaxed);
}

/// Record that a contract poll just succeeded.
pub fn mark_poll_success() {
    LAST_POLL_SUCCESS.store(now_secs(), Ordering::Relaxed);
}

/// Ready when a poll succeeded within the last 2 × poll interval.
fn is_ready() -> bool {
    let last = LAST_POLL_SUCCESS.load(Ordering::Relaxed);
    let interval = POLL_INTERVAL_SECS.load(Ordering::Relaxed).max(1);
    last != 0 && now_secs().saturating_sub(last) <= 2 * interval
}

// ── HTTP endpoint ─────────────────────────────────────────────────────────────

/// Serve the Prometheus `/metrics` endpoint on `addr`.
/// Spawns a background task and returns immediately.
pub async fn serve_metrics(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind metrics endpoint on {}", addr))?;

    tracing::info!(addr = %addr, "Prometheus /metrics endpoint listening");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(handle_metrics))
                    .await;
            });
        }
    });

    Ok(())
}

async fn handle_metrics<B>(
    req: Request<B>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    Ok(route(req.method(), req.uri().path()))
}

fn route(method: &hyper::Method, path: &str) -> Response<Full<Bytes>> {
    if !matches!(path, "/metrics" | "/healthz" | "/readyz") {
        return text_response(StatusCode::NOT_FOUND, "not found\n");
    }
    if method != hyper::Method::GET {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    }
    match path {
        "/healthz" => text_response(StatusCode::OK, "ok\n"),
        "/readyz" if is_ready() => text_response(StatusCode::OK, "ready\n"),
        "/readyz" => text_response(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"),
        _ => {
            use prometheus::Encoder;
            let encoder = prometheus::TextEncoder::new();
            let mut buf = Vec::new();
            encoder
                .encode(&prometheus::gather(), &mut buf)
                .unwrap_or_default();
            let mut resp = Response::new(Full::new(Bytes::from(buf)));
            if let Ok(value) = HeaderValue::from_str(encoder.format_type()) {
                resp.headers_mut().insert(CONTENT_TYPE, value);
            }
            resp
        }
    }
}

fn text_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::Method;

    async fn get(method: Method, path: &str) -> (StatusCode, String) {
        let resp = route(&method, path);
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn metrics_path_serves_prometheus_text() {
        inc_transactions(1);
        let (status, body) = get(Method::GET, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("txwatch_transactions_total"));
    }

    #[tokio::test]
    async fn unknown_path_is_404_and_wrong_method_is_405() {
        assert_eq!(get(Method::GET, "/").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get(Method::GET, "/anything").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            get(Method::POST, "/metrics").await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn healthz_is_always_ok() {
        assert_eq!(
            get(Method::GET, "/healthz").await,
            (StatusCode::OK, "ok\n".into())
        );
    }

    #[tokio::test]
    async fn readyz_tracks_recent_successful_poll() {
        set_poll_interval(30);
        LAST_POLL_SUCCESS.store(0, Ordering::Relaxed);
        assert_eq!(
            get(Method::GET, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );

        mark_poll_success();
        assert_eq!(get(Method::GET, "/readyz").await.0, StatusCode::OK);

        // Older than 2 × interval: stale again.
        LAST_POLL_SUCCESS.store(now_secs() - 61, Ordering::Relaxed);
        assert_eq!(
            get(Method::GET, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
