//! A fake GitHub REST API for the write-back (KAIROS-T-0386): a local axum
//! server that records each request and answers as GitHub does for the 4
//! routes that Kairos calls. No request leaves the host.
//!
//! - `GET /repos/{owner}/{repo}/pulls/{number}`: the pull request, with
//!   the head commit of [`GithubHttp::set_head`].
//! - `POST /repos/{owner}/{repo}/issues/{number}/comments`: 201 with a new
//!   comment id.
//! - `PATCH /repos/{owner}/{repo}/issues/comments/{id}`: 200, or 404 for an
//!   id that [`GithubHttp::remove_comment`] removed.
//! - `POST /repos/{owner}/{repo}/statuses/{sha}`: 201.
//!
//! The issue write-back (KAIROS-T-0575) calls 2 more routes of GitHub, and
//! 2 of GitLab (the GitLab API is on the host of the repository URL):
//!
//! - `PATCH /repos/{owner}/{repo}/issues/{number}`: 200, the issue.
//! - `POST /api/v4/projects/{id}/issues/{iid}/notes`: 201 with a note id.
//! - `PUT /api/v4/projects/{id}/issues/{iid}`: 200, the issue.
//!
//! [`GithubHttp::fail_next`] answers 500 to the next requests, and
//! [`GithubHttp::refuse_next`] answers 403.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use serde_json::{Value, json};

/// One request of the fake API.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub authorization: String,
    pub body: Value,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<Vec<Call>>,
    next_id: AtomicI64,
    fail_next: AtomicUsize,
    refuse_next: AtomicUsize,
    fail_on: Mutex<Option<(String, String, usize)>>,
    heads: Mutex<HashMap<i64, String>>,
    removed: Mutex<HashSet<i64>>,
    comments: Mutex<HashMap<i64, String>>,
}

/// A running fake API.
#[derive(Clone)]
pub struct GithubHttp {
    inner: Arc<Inner>,
    /// `http://127.0.0.1:<port>`: the value of `KAIROS_GITHUB_API_URL`.
    pub url: String,
}

impl GithubHttp {
    /// Each request so far.
    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls.lock().unwrap().clone()
    }

    /// Answer 500 to the next `n` requests.
    pub fn fail_next(&self, n: usize) {
        self.inner.fail_next.store(n, Ordering::SeqCst);
    }

    /// Answer 500 to the next `n` requests of `method` on the path `path`.
    pub fn fail_next_on(&self, method: &str, path: &str, n: usize) {
        *self.inner.fail_on.lock().unwrap() = Some((method.to_string(), path.to_string(), n));
    }

    /// Answer 403 to the next `n` requests.
    pub fn refuse_next(&self, n: usize) {
        self.inner.refuse_next.store(n, Ordering::SeqCst);
    }

    /// The head commit of the pull request `number`.
    pub fn set_head(&self, number: i64, sha: &str) {
        self.inner
            .heads
            .lock()
            .unwrap()
            .insert(number, sha.to_string());
    }

    /// A person removes the comment `id` on GitHub.
    pub fn remove_comment(&self, id: i64) {
        self.inner.removed.lock().unwrap().insert(id);
    }

    /// The body of the comment `id` now.
    pub fn comment(&self, id: i64) -> Option<String> {
        self.inner.comments.lock().unwrap().get(&id).cloned()
    }
}

/// Record the request. `Some`: answer it with this refusal.
fn refusal(
    inner: &Inner,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &str,
) -> Option<Response> {
    inner.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_string(),
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
        body: serde_json::from_str(body).unwrap_or(Value::Null),
    });
    let left = inner.refuse_next.load(Ordering::SeqCst);
    if left > 0 {
        inner.refuse_next.store(left - 1, Ordering::SeqCst);
        return Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"message": "Resource not accessible by personal access token"})),
            )
                .into_response(),
        );
    }
    let left = inner.fail_next.load(Ordering::SeqCst);
    if left > 0 {
        inner.fail_next.store(left - 1, Ordering::SeqCst);
        return Some(failed());
    }
    let mut fail_on = inner.fail_on.lock().unwrap();
    if let Some((failing, path, left)) = fail_on.as_mut()
        && failing.as_str() == method.as_str()
        && path.as_str() == uri.path()
        && *left > 0
    {
        *left -= 1;
        return Some(failed());
    }
    None
}

/// The private token of a GitLab request.
fn private_token(headers: &HeaderMap) -> String {
    headers
        .get("private-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// `PATCH /repos/{owner}/{repo}/issues/{number}` (GitHub) and
/// `PUT /api/v4/projects/{id}/issues/{iid}` (GitLab): the issue.
async fn edit_issue(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let mut headers = headers;
    if !headers.contains_key("authorization") {
        let token = private_token(&headers);
        if let Ok(value) = format!("PRIVATE-TOKEN {token}").parse() {
            headers.insert("authorization", value);
        }
    }
    if let Some(refused) = refusal(&inner, &method, &uri, &headers, &body) {
        return refused;
    }
    axum::Json(json!({ "id": 1 })).into_response()
}

/// `POST /api/v4/projects/{id}/issues/{iid}/notes` (GitLab): a new note.
async fn create_note(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let mut headers = headers;
    let token = private_token(&headers);
    if let Ok(value) = format!("PRIVATE-TOKEN {token}").parse() {
        headers.insert("authorization", value);
    }
    create_comment(State(inner), method, uri, headers, body).await
}

fn failed() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"message": "Server Error"})),
    )
        .into_response()
}

async fn pull(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path((_owner, _repo, number)): Path<(String, String, i64)>,
) -> Response {
    if let Some(refused) = refusal(&inner, &method, &uri, &headers, "") {
        return refused;
    }
    let sha = inner
        .heads
        .lock()
        .unwrap()
        .get(&number)
        .cloned()
        .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_string());
    axum::Json(json!({ "number": number, "state": "open", "head": { "sha": sha } })).into_response()
}

async fn create_comment(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(refused) = refusal(&inner, &method, &uri, &headers, &body) {
        return refused;
    }
    let id = 1000 + inner.next_id.fetch_add(1, Ordering::SeqCst);
    let text = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)["body"]
        .as_str()
        .unwrap_or("")
        .to_string();
    inner.comments.lock().unwrap().insert(id, text);
    (StatusCode::CREATED, axum::Json(json!({ "id": id }))).into_response()
}

async fn edit_comment(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path((_owner, _repo, id)): Path<(String, String, i64)>,
    body: String,
) -> Response {
    if let Some(refused) = refusal(&inner, &method, &uri, &headers, &body) {
        return refused;
    }
    if inner.removed.lock().unwrap().contains(&id) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message": "Not Found"})),
        )
            .into_response();
    }
    let text = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)["body"]
        .as_str()
        .unwrap_or("")
        .to_string();
    inner.comments.lock().unwrap().insert(id, text);
    axum::Json(json!({ "id": id })).into_response()
}

async fn status(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(refused) = refusal(&inner, &method, &uri, &headers, &body) {
        return refused;
    }
    (StatusCode::CREATED, axum::Json(json!({ "id": 1 }))).into_response()
}

/// Start a fake API on an ephemeral port of 127.0.0.1.
pub async fn serve() -> GithubHttp {
    let inner = Arc::new(Inner::default());
    let app = Router::new()
        .route("/repos/{owner}/{repo}/pulls/{number}", get(pull))
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            post(create_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/comments/{id}",
            patch(edit_comment),
        )
        .route("/repos/{owner}/{repo}/statuses/{sha}", post(status))
        .route("/repos/{owner}/{repo}/issues/{number}", patch(edit_issue))
        .route("/api/v4/projects/{id}/issues/{iid}", put(edit_issue))
        .route(
            "/api/v4/projects/{id}/issues/{iid}/notes",
            post(create_note),
        )
        .with_state(Arc::clone(&inner));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake GitHub") });
    GithubHttp {
        inner,
        url: format!("http://{addr}"),
    }
}
