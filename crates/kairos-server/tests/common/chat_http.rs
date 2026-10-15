//! A fake OpenAI-compatible chat endpoint (KAIROS-T-0341): `POST
//! /v1/chat/completions` on an ephemeral port. It checks the bearer token,
//! answers a 429 for the first `fail_first` requests, counts the requests
//! and the most requests in flight at one time, and gives one fixed
//! summary for each prompt: "A hosted summary of `<name>`.", with the name
//! from the first pair of backticks of the user prompt.
//!
//! KAIROS-T-0495: [`serve_refusing`] gives each leading 429 its own
//! `Retry-After` (or none), and can answer 429 to each request until
//! [`ChatHttp::stop_refusing`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

#[derive(Clone)]
struct Shared {
    key: String,
    /// The leading 429s, each with its `Retry-After`.
    refusals: Arc<Mutex<VecDeque<Option<String>>>>,
    /// A 429 for each request while it is set.
    always: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

/// The fake endpoint, and what it saw.
pub struct ChatHttp {
    /// The base URL of the endpoint: `http://127.0.0.1:<port>/v1`.
    pub base_url: String,
    requests: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    always: Arc<AtomicBool>,
}

impl ChatHttp {
    /// Answer the next requests normally.
    #[allow(dead_code)]
    pub fn stop_refusing(&self) {
        self.always.store(false, Ordering::SeqCst);
    }

    /// The requests that reached the endpoint, the refused ones included.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The most requests that were in flight at one time.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

/// Serve the fake endpoint. `key` is the bearer token it takes;
/// `fail_first` requests get a 429 before the endpoint answers.
pub async fn serve(key: &str, fail_first: usize) -> ChatHttp {
    serve_refusing(key, vec![None; fail_first], false).await
}

/// Serve the fake endpoint: a 429 for each of `refusals`, with its
/// `Retry-After` when it has one, then a 429 for each request while
/// `always`.
pub async fn serve_refusing(key: &str, refusals: Vec<Option<&str>>, always: bool) -> ChatHttp {
    let shared = Shared {
        key: key.to_string(),
        refusals: Arc::new(Mutex::new(
            refusals
                .into_iter()
                .map(|r| r.map(str::to_string))
                .collect(),
        )),
        always: Arc::new(AtomicBool::new(always)),
        requests: Arc::new(AtomicUsize::new(0)),
        in_flight: Arc::new(AtomicUsize::new(0)),
        max_in_flight: Arc::new(AtomicUsize::new(0)),
    };
    let requests = shared.requests.clone();
    let max_in_flight = shared.max_in_flight.clone();
    let always = shared.always.clone();
    let router = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("chat server");
    });
    ChatHttp {
        base_url: format!("http://{addr}/v1"),
        requests,
        max_in_flight,
        always,
    }
}

async fn complete(State(shared): State<Shared>, headers: HeaderMap, body: String) -> Response {
    shared.requests.fetch_add(1, Ordering::SeqCst);
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if bearer != format!("Bearer {}", shared.key) {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"error": {"message": "invalid api key"}}).to_string(),
        )
            .into_response();
    }
    let refusal = shared.refusals.lock().unwrap().pop_front();
    if refusal.is_some() || shared.always.load(Ordering::SeqCst) {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "too many concurrent requests"}).to_string(),
        )
            .into_response();
        if let Some(value) = refusal.flatten() {
            response
                .headers_mut()
                .insert("retry-after", value.parse().unwrap());
        }
        return response;
    }
    let now = shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    shared.max_in_flight.fetch_max(now, Ordering::SeqCst);
    // Long enough for the requests of one batch to overlap.
    tokio::time::sleep(Duration::from_millis(40)).await;
    shared.in_flight.fetch_sub(1, Ordering::SeqCst);
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let prompt = parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_str())
        .unwrap_or("");
    let name = prompt.split('`').nth(1).unwrap_or("something").to_string();
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion",
            "model": parsed["model"],
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": format!("A hosted summary of `{name}`.")},
                "finish_reason": "stop",
            }],
        })
        .to_string(),
    )
        .into_response()
}
