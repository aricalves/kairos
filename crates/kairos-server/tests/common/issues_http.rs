//! A fake list of issues of GitHub and of GitLab for the import
//! (KAIROS-T-0483): a local axum server that answers from recorded fixtures,
//! page by page, and records each request. No request leaves the host.
//!
//! - `GET /repos/{owner}/{repo}/issues` (GitHub),
//! - `GET /api/v4/projects/{id}/issues` (GitLab).
//!
//! [`IssuesHttp::refuse`] answers 401; [`IssuesHttp::rate_limit_at`]
//! answers 403 with `x-ratelimit-remaining: 0` from that page on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

/// One request.
#[derive(Debug, Clone)]
pub struct Call {
    pub path_and_query: String,
    pub authorization: String,
    pub private_token: String,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<Vec<Call>>,
    github: Mutex<Vec<Value>>,
    gitlab: Mutex<Vec<Value>>,
    refuse: AtomicBool,
    /// 0: no rate limit.
    rate_limit_at: AtomicUsize,
}

/// A running fake API.
#[derive(Clone)]
pub struct IssuesHttp {
    inner: Arc<Inner>,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
}

impl IssuesHttp {
    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls.lock().unwrap().clone()
    }
    pub fn set_github(&self, rows: Vec<Value>) {
        *self.inner.github.lock().unwrap() = rows;
    }
    pub fn set_gitlab(&self, rows: Vec<Value>) {
        *self.inner.gitlab.lock().unwrap() = rows;
    }
    pub fn refuse(&self, on: bool) {
        self.inner.refuse.store(on, Ordering::SeqCst);
    }
    pub fn rate_limit_at(&self, page: usize) {
        self.inner.rate_limit_at.store(page, Ordering::SeqCst);
    }
}

fn page(rows: &[Value], query: &HashMap<String, String>) -> Vec<Value> {
    let per_page: usize = query
        .get("per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let page: usize = query.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    rows.iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .cloned()
        .collect()
}

fn answer(
    inner: &Inner,
    uri: &Uri,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    rows: Vec<Value>,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    inner.calls.lock().unwrap().push(Call {
        path_and_query: uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default(),
        authorization: header("authorization"),
        private_token: header("private-token"),
    });
    if inner.refuse.load(Ordering::SeqCst) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"message": "Bad credentials"})),
        )
            .into_response();
    }
    let limit = inner.rate_limit_at.load(Ordering::SeqCst);
    let page_no: usize = query.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    if limit > 0 && page_no >= limit {
        return (
            StatusCode::FORBIDDEN,
            [("x-ratelimit-remaining", "0")],
            axum::Json(json!({"message": "API rate limit exceeded"})),
        )
            .into_response();
    }
    axum::Json(Value::Array(page(&rows, query))).into_response()
}

async fn github(
    State(inner): State<Arc<Inner>>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let rows = inner.github.lock().unwrap().clone();
    answer(&inner, &uri, &headers, &query, rows)
}

async fn gitlab(
    State(inner): State<Arc<Inner>>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let rows = inner.gitlab.lock().unwrap().clone();
    answer(&inner, &uri, &headers, &query, rows)
}

/// Start a fake API on an ephemeral port of 127.0.0.1.
pub async fn serve() -> IssuesHttp {
    let inner = Arc::new(Inner::default());
    let app = Router::new()
        .route("/repos/{owner}/{repo}/issues", get(github))
        .route("/api/v4/projects/{id}/issues", get(gitlab))
        .with_state(Arc::clone(&inner));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding the fake issues API");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake issues API");
    });
    IssuesHttp { inner, url }
}
