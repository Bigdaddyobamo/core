//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes, labelled by `contract` (label) and `network` where noted:
//! - `txwatch_transactions_total{contract,network}`      — transactions processed
//! - `txwatch_alerts_total{contract,network}`            — alert payloads sent (rules matched)
//! - `txwatch_webhook_failures_total{contract,network}`  — permanent webhook delivery failures
//! - `txwatch_horizon_request_duration_seconds{network}` — Horizon request latency
//! - `txwatch_webhook_delivery_duration_seconds`         — webhook delivery latency (incl. retries)
//! - `txwatch_last_successful_poll_timestamp_seconds{contract,network}` — data freshness
//! - `txwatch_consecutive_poll_failures{contract,network}` — current failure streak
//! - `txwatch_build_info{version,git_sha}`               — always 1
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
use prometheus::{
    register_histogram, register_histogram_vec, register_int_counter_vec, register_int_gauge_vec,
    Histogram, HistogramVec, IntCounterVec, IntGaugeVec,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use http_body_util::Full;
use hyper::{
    body::Bytes,
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, sync::watch};

// ── Metrics ───────────────────────────────────────────────────────────────────

const CONTRACT_LABELS: &[&str] = &["contract", "network"];

fn transactions_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_transactions_total",
            "Total Stellar transactions processed, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_transactions_total")
    })
}

fn alerts_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_alerts_total",
            "Total alert payloads sent (rules matched), per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_alerts_total")
    })
}

fn webhook_failures_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_webhook_failures_total",
            "Total permanent webhook delivery failures (after all retries), per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_webhook_failures_total")
    })
}

fn horizon_request_duration() -> &'static HistogramVec {
    static H: OnceLock<HistogramVec> = OnceLock::new();
    H.get_or_init(|| {
        register_histogram_vec!(
            "txwatch_horizon_request_duration_seconds",
            "Duration of Horizon HTTP requests",
            &["network"]
        )
        .expect("register txwatch_horizon_request_duration_seconds")
    })
}

fn webhook_delivery_duration() -> &'static Histogram {
    static H: OnceLock<Histogram> = OnceLock::new();
    H.get_or_init(|| {
        register_histogram!(
            "txwatch_webhook_delivery_duration_seconds",
            "Duration of webhook deliveries, including retries"
        )
        .expect("register txwatch_webhook_delivery_duration_seconds")
    })
}

fn last_successful_poll() -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge_vec!(
            "txwatch_last_successful_poll_timestamp_seconds",
            "Unix time of the last successful poll, per watched contract",
            CONTRACT_LABELS
        )
        .expect("register txwatch_last_successful_poll_timestamp_seconds")
    })
}

fn consecutive_poll_failures() -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge_vec!(
            "txwatch_consecutive_poll_failures",
            "Consecutive failed polls, per watched contract (0 after a success)",
            CONTRACT_LABELS
        )
        .expect("register txwatch_consecutive_poll_failures")
    })
}

/// Registers `txwatch_build_info{version,git_sha} 1`. `git_sha` comes from the
/// `TXWATCH_GIT_SHA` environment variable at build time, if set.
pub fn register_build_info() {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        let gauge = register_int_gauge_vec!(
            "txwatch_build_info",
            "Build information; the value is always 1",
            &["version", "git_sha"]
        )
        .expect("register txwatch_build_info");
        gauge
            .with_label_values(&[
                env!("CARGO_PKG_VERSION"),
                option_env!("TXWATCH_GIT_SHA").unwrap_or("unknown"),
            ])
            .set(1);
        gauge
    });
}

/// Increment `txwatch_transactions_total` for a contract by `n`.
pub fn inc_transactions(contract: &str, network: &str, n: u64) {
    transactions_total()
        .with_label_values(&[contract, network])
        .inc_by(n);
}

/// Increment `txwatch_alerts_total` for a contract by `n`.
pub fn inc_alerts(contract: &str, network: &str, n: u64) {
    alerts_total()
        .with_label_values(&[contract, network])
        .inc_by(n);
}

/// Increment `txwatch_webhook_failures_total` for a contract by 1.
pub fn inc_webhook_failures(contract: &str, network: &str) {
    webhook_failures_total()
        .with_label_values(&[contract, network])
        .inc();
}

/// Record how long a Horizon request on `network` took.
pub fn observe_horizon_request(network: &str, seconds: f64) {
    horizon_request_duration()
        .with_label_values(&[network])
        .observe(seconds);
}

/// Record how long one webhook delivery (including retries) took.
pub fn observe_webhook_delivery(seconds: f64) {
    webhook_delivery_duration().observe(seconds);
}

/// Record a successful poll of a contract: freshness timestamp and reset
/// failure streak.
pub fn record_poll_success(contract: &str, network: &str) {
    last_successful_poll()
        .with_label_values(&[contract, network])
        .set(now_secs() as i64);
    consecutive_poll_failures()
        .with_label_values(&[contract, network])
        .set(0);
}

/// Record a failed poll of a contract.
pub fn record_poll_failure(contract: &str, network: &str) {
    consecutive_poll_failures()
        .with_label_values(&[contract, network])
        .inc();
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

/// Record that a contract poll just succeeded (for readiness).
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

/// First pause after a failed `accept`, doubled on each consecutive failure.
const ACCEPT_BACKOFF_START: Duration = Duration::from_millis(100);
/// Longest pause between `accept` retries.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Minimum gap between two "accept failed" warnings.
const ACCEPT_WARN_EVERY: Duration = Duration::from_secs(10);

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(ACCEPT_BACKOFF_MAX)
}

/// Serve `/metrics`, `/healthz` and `/readyz` on `addr` until `shutdown`
/// becomes `true`. Spawns a background task and returns the bound address
/// (useful when `addr` uses port 0).
///
/// A failing `accept` (e.g. `EMFILE`) is retried after a growing pause instead
/// of spinning, and logged at warn level at most once every 10 seconds.
pub async fn serve_metrics(
    addr: SocketAddr,
    mut shutdown: watch::Receiver<bool>,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind metrics endpoint on {}", addr))?;
    let local_addr = listener.local_addr().context("metrics listener address")?;

    tracing::info!(addr = %local_addr, "Prometheus /metrics endpoint listening");

    tokio::spawn(async move {
        let mut backoff = ACCEPT_BACKOFF_START;
        let mut last_warn: Option<Instant> = None;
        // Once the sender is dropped no shutdown can arrive; stop watching.
        let mut watching = true;
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        backoff = ACCEPT_BACKOFF_START;
                        let io = TokioIo::new(stream);
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, hyper::service::service_fn(handle_metrics))
                                .await;
                        });
                    }
                    Err(e) => {
                        if last_warn.is_none_or(|t| t.elapsed() >= ACCEPT_WARN_EVERY) {
                            tracing::warn!(
                                error = %e,
                                retry_in_ms = backoff.as_millis() as u64,
                                "metrics endpoint failed to accept a connection"
                            );
                            last_warn = Some(Instant::now());
                        }
                        tokio::select! {
                            () = tokio::time::sleep(backoff) => {}
                            _ = shutdown.changed(), if watching => {}
                        }
                        backoff = next_backoff(backoff);
                    }
                },
                changed = shutdown.changed(), if watching => {
                    if changed.is_err() {
                        watching = false;
                    }
                }
            }
        }
        tracing::info!("metrics endpoint stopped");
    });

    Ok(local_addr)
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
        inc_transactions("Test", "testnet", 1);
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

#[cfg(test)]
mod server_tests {
    use super::*;

    #[test]
    fn accept_backoff_grows_and_caps() {
        assert_eq!(
            next_backoff(ACCEPT_BACKOFF_START),
            Duration::from_millis(200)
        );
        assert_eq!(next_backoff(Duration::from_secs(4)), ACCEPT_BACKOFF_MAX);
        assert_eq!(next_backoff(ACCEPT_BACKOFF_MAX), ACCEPT_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn server_stops_accepting_after_shutdown() {
        let (tx, rx) = watch::channel(false);
        let addr = serve_metrics("127.0.0.1:0".parse().unwrap(), rx)
            .await
            .unwrap();
        assert!(tokio::net::TcpStream::connect(addr).await.is_ok());

        tx.send(true).unwrap();
        // The accept task exits and drops the listener.
        let mut refused = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(addr).await.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "listener should close after shutdown");
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    fn exposition() -> String {
        use prometheus::Encoder;
        let mut buf = Vec::new();
        prometheus::TextEncoder::new()
            .encode(&prometheus::gather(), &mut buf)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn per_contract_metrics_carry_labels() {
        register_build_info();
        inc_transactions("LabelTest", "testnet", 3);
        inc_alerts("LabelTest", "testnet", 1);
        inc_webhook_failures("LabelTest", "testnet");
        observe_horizon_request("testnet", 0.25);
        observe_webhook_delivery(0.5);
        record_poll_failure("LabelTest", "testnet");
        record_poll_failure("LabelTest", "testnet");

        let text = exposition();
        assert!(text
            .contains(r#"txwatch_transactions_total{contract="LabelTest",network="testnet"} 3"#));
        assert!(text.contains(r#"txwatch_alerts_total{contract="LabelTest",network="testnet"} 1"#));
        assert!(text.contains(
            r#"txwatch_webhook_failures_total{contract="LabelTest",network="testnet"} 1"#
        ));
        assert!(text.contains(
            r#"txwatch_consecutive_poll_failures{contract="LabelTest",network="testnet"} 2"#
        ));
        assert!(text.contains("txwatch_horizon_request_duration_seconds_bucket"));
        assert!(text.contains("txwatch_webhook_delivery_duration_seconds_count"));
        assert!(text.contains(&format!(
            r#"txwatch_build_info{{git_sha="{}",version="{}"}} 1"#,
            option_env!("TXWATCH_GIT_SHA").unwrap_or("unknown"),
            env!("CARGO_PKG_VERSION")
        )));

        record_poll_success("LabelTest", "testnet");
        let text = exposition();
        assert!(text.contains(
            r#"txwatch_consecutive_poll_failures{contract="LabelTest",network="testnet"} 0"#
        ));
        assert!(text.contains(r#"txwatch_last_successful_poll_timestamp_seconds{contract="LabelTest",network="testnet"}"#));
    }
}
