//! The data plane: a streaming reverse proxy that forwards requests to
//! whichever backend a `LoadBalancer` picks. The backend list is shared
//! state that the orchestrator/autoscaler mutate as instances come and go;
//! the proxy itself never spawns or stops containers.

mod metrics;

pub use metrics::{ActiveGuard, MetricsRegistry};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::Stream;
use lw_orchestrator::Instance;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, warn};

/// Picks a backend from the current pool. Implementations must be cheap
/// and non-blocking since this runs on the hot path for every request.
pub trait LoadBalancer: Send + Sync {
    fn pick(&self, backends: &[Instance]) -> Option<Instance>;
}

/// Simple round robin: cycles through the pool in order.
#[derive(Default)]
pub struct RoundRobin {
    counter: AtomicUsize,
}

impl LoadBalancer for RoundRobin {
    fn pick(&self, backends: &[Instance]) -> Option<Instance> {
        if backends.is_empty() {
            return None;
        }
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % backends.len();
        backends.get(idx).cloned()
    }
}

/// Shared, mutable list of healthy backends for one service. The
/// autoscaler/orchestrator call `set()` after spawning/stopping/health
/// checking instances; the proxy only ever reads from it.
#[derive(Clone, Default)]
pub struct BackendPool {
    inner: Arc<RwLock<Vec<Instance>>>,
}

impl BackendPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn set(&self, backends: Vec<Instance>) {
        *self.inner.write().await = backends;
    }

    pub async fn get(&self) -> Vec<Instance> {
        self.inner.read().await.clone()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[derive(Clone)]
pub struct ProxyState {
    pub pool: BackendPool,
    pub lb: Arc<dyn LoadBalancer>,
    pub http: reqwest::Client,
    pub metrics: MetricsRegistry,
}

impl ProxyState {
    pub fn new(
        pool: BackendPool,
        lb: Arc<dyn LoadBalancer>,
        metrics: MetricsRegistry,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            // No fixed timeout: exam-card downloads can legitimately be
            // large/slow, and we don't want the proxy to cut them off.
            // Add a per-route timeout later if a service needs one.
            .build()?;
        Ok(Self { pool, lb, http, metrics })
    }
}

pub fn router(state: ProxyState) -> Router {
    Router::new()
        .route("/*path", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state)
}

async fn proxy_handler(State(state): State<ProxyState>, req: Request) -> Response {
    let backends = state.pool.get().await;
    let Some(backend) = state.lb.pick(&backends) else {
        warn!("no healthy backends available");
        return (StatusCode::SERVICE_UNAVAILABLE, "no healthy backends available").into_response();
    };

    // Held for the whole request, including body streaming below — see the
    // doc comment on ActiveGuard for why that matters for slow downloads.
    let active_guard = state.metrics.begin_active(&backend.container_id);
    let started = Instant::now();

    match forward(&state.http, &backend, req).await {
        Ok(resp) => {
            // Time-to-first-byte: recorded once we have upstream headers,
            // not after the full (possibly large) body has streamed out.
            state.metrics.record_latency(&backend.container_id, started.elapsed());
            attach_guard(resp, active_guard)
        }
        Err(e) => {
            error!(backend = %backend.base_url(), error = %e, "error forwarding request");
            drop(active_guard);
            (StatusCode::BAD_GATEWAY, "upstream request failed").into_response()
        }
    }
}

/// Streams the incoming request to `backend` and streams its response back,
/// so a large download (e.g. an exam card PDF) never gets buffered fully
/// into memory on either leg.
async fn forward(
    http: &reqwest::Client,
    backend: &Instance,
    req: Request,
) -> anyhow::Result<Response> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let target_url = build_target_url(backend, &uri);

    let body_stream = req.into_body().into_data_stream();
    let outbound_body = reqwest::Body::wrap_stream(body_stream);

    let mut builder = http.request(method, target_url).body(outbound_body);
    for (name, value) in filtered_headers(&headers) {
        builder = builder.header(name, value);
    }

    let upstream_resp = builder.send().await?;

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let resp_stream = upstream_resp.bytes_stream();

    let mut response = Response::builder().status(status.as_u16());
    if let Some(h) = response.headers_mut() {
        *h = resp_headers;
    }
    let response = response.body(Body::from_stream(resp_stream))?;
    Ok(response)
}

/// Rebuilds the response with its body wrapped so `guard` stays alive
/// (and the active-connection count stays incremented) until the client
/// has finished receiving the body — not just until headers went out.
fn attach_guard(resp: Response, guard: ActiveGuard) -> Response {
    let (parts, body) = resp.into_parts();
    let guarded = GuardedStream { inner: body.into_data_stream(), _guard: guard };
    Response::from_parts(parts, Body::from_stream(guarded))
}

struct GuardedStream<S> {
    inner: S,
    _guard: ActiveGuard,
}

impl<S> Stream for GuardedStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn build_target_url(backend: &Instance, uri: &Uri) -> String {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    format!("{}{}", backend.base_url(), path_and_query)
}

/// Strips hop-by-hop headers that shouldn't be forwarded as-is (notably
/// `host`, which must reflect the backend, not the original request).
fn filtered_headers(headers: &HeaderMap) -> impl Iterator<Item = (&axum::http::HeaderName, &axum::http::HeaderValue)> {
    const SKIP: &[&str] = &["host", "connection", "content-length"];
    headers
        .iter()
        .filter(|(name, _)| !SKIP.contains(&name.as_str()))
}
