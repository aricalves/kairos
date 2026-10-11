//! A test receiver of webhook deliveries (KAIROS-T-0384, KAIROS-T-0385): a
//! local axum server that records each request (headers, body) and answers
//! 204, or 500 when it is told to fail.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;

/// One request that the receiver got.
#[derive(Debug, Clone)]
pub struct Received {
    /// The headers, with lower-case names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Received {
    /// The body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

#[derive(Default)]
struct Inner {
    received: Mutex<Vec<Received>>,
    fail: AtomicBool,
}

/// A running receiver.
#[derive(Clone)]
pub struct HookHttp {
    inner: Arc<Inner>,
    /// `http://127.0.0.1:<port>/hook`.
    pub url: String,
}

impl HookHttp {
    /// Each request so far.
    pub fn received(&self) -> Vec<Received> {
        self.inner.received.lock().unwrap().clone()
    }

    /// Answer 500 (true) or 204 (false).
    pub fn fail(&self, fail: bool) {
        self.inner.fail.store(fail, Ordering::SeqCst);
    }
}

async fn receive(State(inner): State<Arc<Inner>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    inner.received.lock().unwrap().push(Received {
        headers: headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: body.to_vec(),
    });
    if inner.fail.load(Ordering::SeqCst) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::NO_CONTENT
    }
}

/// Start a receiver on an ephemeral port of 127.0.0.1.
pub async fn serve() -> HookHttp {
    let inner = Arc::new(Inner::default());
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(Arc::clone(&inner));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("receiver") });
    HookHttp {
        inner,
        url: format!("http://{addr}/hook"),
    }
}
