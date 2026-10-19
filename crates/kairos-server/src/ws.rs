//! `GET /ws/events` — the tenant-scoped WebSocket event channel
//! (KAIROS-T-0022, contract per KAIROS-A-0005 §5 / KAIROS-S-0005 "Event
//! Push").
//!
//! # Fan-out
//!
//! One dedicated LISTEN connection per server process ([`spawn_listener`],
//! raw `tokio-postgres` — the pooled diesel connections cannot sit on a
//! long-lived `LISTEN`) receives every `NOTIFY kairos_events`, parses the
//! tenant-tagged events of the payload, and forwards each one into the
//! `tokio::sync::broadcast` channel of its tenant (KAIROS-T-0621: one
//! channel for each tenant). Each connected socket subscribes to the
//! channel of ITS tenant (bound at upgrade time from the resolved
//! [`TenantContext`]). A client message filters the stream:
//! `{"subscribe": {"board_id": "..."}}` to one board, or
//! `{"subscribe": {"person_only": true}}` to the events of the person.
//! The message of an event is `Utf8Bytes`: each socket clones a reference,
//! not the text.
//! The board filter also passes an event about an item on a different
//! board when that item has a `blocks` edge to or from a live item on the
//! subscribed board (COLLIERY-T-0233): the event changes the counts on
//! that board's cards. [`BroadcastEvent::concerns_board`] is the rule.
//!
//! # The emitter (KAIROS-T-0632)
//!
//! The mutating services write their events to the outbox
//! (`kairos_db::events`). The owner of the LISTEN connection also runs the
//! emitter of the process ([`spawn_emitter`]): on a connection of its own,
//! it sends the unsent rows every [`EMIT_POLL`], one `NOTIFY` for each
//! batch, and prunes the sent rows every [`PRUNE_EVERY`]. A payload is a
//! JSON array of events. One JSON object is one event: the shape before
//! the outbox, which a server of an older version still sends.
//!
//! An event of a person (`notification_created`, `notifications_read`,
//! KAIROS-T-0381) carries the routing field `recipients`: a socket gets it
//! only when the person of the socket (the authenticated user of the
//! upgrade request) is in the list. The field `recipient` (one person) of
//! an older server still parses. [`BroadcastEvent::visible_to`] is the
//! rule, and it comes after the tenant check. The field is stripped, as
//! `tenant` is. The event has no board. A board filter still passes it
//! (KAIROS-T-0621): the bell and the board view of a tab share one socket.
//! The filter `person_only` passes only these events.
//!
//! If the LISTEN connection drops it is re-established with exponential
//! backoff (logged, with a reconnect counter on the [`EventHub`]).
//!
//! # Authentication (same middleware, same semantics)
//!
//! The upgrade request passes through the standard stack —
//! [`crate::middleware::auth::require_auth`] then
//! [`crate::middleware::tenant::require_tenant`] — so a missing/invalid
//! token is the usual 401, a non-member the usual 403, and an unknown
//! tenant the usual 404, all BEFORE the protocol upgrade. Tokens travel in
//! the `Authorization: Bearer` header; because browser `WebSocket` clients
//! cannot set request headers, the ONE supported fallback is the
//! `?access_token=<jwt>` query parameter, which [`promote_query_token`]
//! copies into the `Authorization` header ahead of the auth layer (browser
//! clients resolve their tenant via the `Host` subdomain, A-0005 §2).
//!
//! The socket checks its credential again every [`SESSION_RECHECK`]
//! (KAIROS-T-0621). When the credential is no longer valid, the server
//! closes the socket with the code 1008. The client then connects again
//! with its new credential.
//!
//! # Best-effort semantics (A-0005 §5)
//!
//! Events are thin UI-freshness hints, not a durable stream: NO replay, NO
//! ordering guarantee beyond per-connection FIFO. A socket that falls
//! behind the broadcast buffer skips the lagged events. The server then
//! sends the frame [`LAGGED_FRAME`] (KAIROS-T-0621). Clients reconcile by
//! re-fetching through the REST API after that frame and after
//! (re)connecting.
//! Disconnects are clean by construction: the per-socket task is the only
//! resource, and it ends when the socket does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::{self as axum_middleware, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::app::AppState;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;
use crate::middleware::{auth, tenant};

/// The NOTIFY channel the db services emit on (`kairos_db::events`).
const EVENT_CHANNEL: &str = "kairos_events";

/// Broadcast buffer per tenant. Sockets that fall more than this many
/// events behind skip the overwritten ones and get [`LAGGED_FRAME`]
/// (best-effort, see module docs).
const BROADCAST_CAPACITY: usize = 1024;

/// The frame that tells a client that it skipped events (KAIROS-T-0621).
/// The client reads again what it shows.
pub const LAGGED_FRAME: &str = r#"{"event":"lagged"}"#;

/// The time between two checks of the credential of a socket
/// (KAIROS-T-0621).
pub const SESSION_RECHECK: Duration = Duration::from_secs(5 * 60);

/// The close code when the credential of a socket is no longer valid:
/// 1008, "policy violation" (RFC 6455 §7.4.1).
const CLOSE_SESSION_ENDED: u16 = 1008;

/// The time between two passes of the emitter when the outbox is empty
/// (KAIROS-T-0632). It bounds the delay of an event.
pub const EMIT_POLL: Duration = Duration::from_millis(100);

/// The time between two prunes of the outbox (KAIROS-T-0632).
pub const PRUNE_EVERY: Duration = Duration::from_secs(10);

/// Reconnect backoff bounds for the LISTEN connection and the emitter.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One parsed NOTIFY payload, ready for fan-out: the routing fields the
/// sockets filter on, plus the client-facing message (tenant stripped).
#[derive(Debug)]
struct BroadcastEvent {
    /// The emitting tenant's slug (fan-out routing; never sent to clients).
    tenant: String,
    /// The event's board, when it has one (client-side subscribe filter).
    board_id: Option<Uuid>,
    /// The boards across the item's `blocks` edges (COLLIERY-T-0233,
    /// `kairos_db::events` module docs). Fan-out routing, like `tenant`:
    /// never sent to clients.
    blocks_boards: Vec<Uuid>,
    /// The persons who may get the event (KAIROS-T-0381, KAIROS-T-0632),
    /// or `None` for an event of the tenant. Fan-out routing: never sent to
    /// clients.
    recipients: Option<Vec<Uuid>>,
    /// The serialized S-0005 thin event forwarded to matching sockets.
    /// `Utf8Bytes` holds `Bytes`: a clone for a socket copies no text.
    message: Utf8Bytes,
}

/// The filter of one socket, from the client subscribe message.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Filter {
    /// Only the events of this board (and of the person).
    board: Option<Uuid>,
    /// Only the events of the person (KAIROS-T-0621). The server ignores
    /// `board` when this is `true`.
    person_only: bool,
}

impl BroadcastEvent {
    /// Whether a socket filtered to `board` gets this event: the item is
    /// on that board, or the item has a `blocks` edge to or from a live
    /// item on that board (COLLIERY-T-0233). The second case keeps the
    /// blocked-by count of a card current when its blocker on a different
    /// board is completed.
    ///
    /// This does not widen what a principal can see. Reads are open
    /// tenant-wide (A-0006), and a socket with no filter gets every event
    /// of the tenant: the filter is a convenience, not an authorization.
    /// The tenant check comes first and does not change.
    fn concerns_board(&self, board: Uuid) -> bool {
        self.board_id == Some(board) || self.blocks_boards.contains(&board)
    }

    /// Whether the socket of `user` may get this event: each event of the
    /// tenant, and an event of a person only for the persons in its list
    /// (KAIROS-T-0381). This IS an authorization: a notification is private.
    fn visible_to(&self, user: Uuid) -> bool {
        self.recipients
            .as_ref()
            .is_none_or(|recipients| recipients.contains(&user))
    }

    /// Whether this is an event of the person `user`.
    fn of_person(&self, user: Uuid) -> bool {
        self.recipients
            .as_ref()
            .is_some_and(|recipients| recipients.contains(&user))
    }

    /// Whether the socket of `user` with `filter` gets this event. The
    /// authorization ([`Self::visible_to`]) comes first. The events of
    /// the person pass each filter: one socket serves the bell and a view
    /// (KAIROS-T-0621).
    fn passes(&self, filter: Filter, user: Uuid) -> bool {
        if !self.visible_to(user) {
            return false;
        }
        let of_person = self.of_person(user);
        if filter.person_only {
            return of_person;
        }
        match filter.board {
            Some(board) => of_person || self.concerns_board(board),
            None => true,
        }
    }
}

/// One broadcast channel of a tenant.
type TenantChannel = broadcast::Sender<Arc<BroadcastEvent>>;

/// The per-process fan-out hub: LISTEN → the channel of the tenant →
/// sockets.
#[derive(Clone)]
pub struct EventHub {
    /// One channel for each tenant with a socket (KAIROS-T-0621). A socket
    /// thus never wakes for the events of a different tenant.
    channels: Arc<Mutex<HashMap<String, TenantChannel>>>,
    /// The buffer of each channel.
    capacity: usize,
    /// How many times the LISTEN connection was (re-)established after the
    /// first attempt — the "reconnects" metric (logged on each increment).
    reconnects: Arc<AtomicU64>,
}

impl EventHub {
    fn new() -> Self {
        Self::with_capacity(BROADCAST_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            reconnects: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A receiver on the channel of `tenant`. The first socket of a tenant
    /// makes the channel.
    fn subscribe(&self, tenant: &str) -> broadcast::Receiver<Arc<BroadcastEvent>> {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        channels
            .entry(tenant.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe()
    }

    /// Send `event` to the sockets of its tenant. A tenant with no socket
    /// gets nothing, and its channel goes.
    fn publish(&self, event: BroadcastEvent) {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(channel) = channels.get(&event.tenant) else {
            return;
        };
        if channel.receiver_count() == 0 {
            channels.remove(&event.tenant);
            return;
        }
        // An error means that the last receiver went just now.
        drop(channel.send(Arc::new(event)));
    }
}

/// Build the `/ws/events` route behind the standard auth → tenant stack
/// (plus the browser token fallback) and start this process's LISTEN
/// task. Merged into the production router by [`crate::app::router`].
pub fn router(state: AppState) -> Router<AppState> {
    let hub = EventHub::new();
    spawn_listener(state.config.database_url.clone(), hub.clone());
    spawn_emitter(state.config.database_url.clone());
    Router::new()
        .route("/ws/events", get(ws_events))
        .layer(Extension(hub))
        // route_layer wraps bottom-up (the layer added LAST runs FIRST):
        // promote_query_token → require_auth → require_tenant — the
        // A-0010 ordering with the browser fallback ahead of it. The layer
        // of COLLIERY-T-0256 is the last to run.
        .route_layer(axum_middleware::from_fn(refuse_unknown_query))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            tenant::require_tenant,
        ))
        .route_layer(axum_middleware::from_fn_with_state(
            state,
            auth::require_auth,
        ))
        .route_layer(axum_middleware::from_fn(promote_query_token))
}

// ---------------------------------------------------------------------------
// Browser token fallback (module docs: the ONE fallback is ?access_token=)
// ---------------------------------------------------------------------------

/// The `access_token` query parameter, if present and non-empty. JWTs are
/// URL-safe by construction, so no percent-decoding is needed.
fn query_access_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("access_token="))
        .filter(|token| !token.is_empty())
}

/// COLLIERY-T-0256: `access_token` is the one query parameter of this
/// route. Each other parameter is refused, as on the routes of the API.
async fn refuse_unknown_query(req: Request, next: Next) -> Response {
    match crate::input::refuse_parameters_not_in(req.uri(), &["access_token"]) {
        Some(refusal) => refusal.into_response(),
        None => next.run(req).await,
    }
}

/// Copy `?access_token=<jwt>` into the `Authorization` header when the
/// header is absent, so the standard [`auth::require_auth`] layer (and its
/// 401 semantics) applies unchanged to browser WebSocket clients.
async fn promote_query_token(mut req: Request, next: Next) -> Response {
    if !req.headers().contains_key(AUTHORIZATION)
        && let Some(token) = req.uri().query().and_then(query_access_token)
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        req.headers_mut().insert(AUTHORIZATION, value);
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// The socket side
// ---------------------------------------------------------------------------

/// The client subscribe message (`kairos_client::types_events`):
/// `{"subscribe": {"board_id": "uuid"}}` filters to one board,
/// `{"subscribe": {"person_only": true}}` to the events of the person,
/// and `{"subscribe": {}}` clears.
#[derive(Debug, serde::Deserialize)]
struct ClientMessage {
    subscribe: Option<SubscribeFilter>,
}

#[derive(Debug, serde::Deserialize)]
struct SubscribeFilter {
    board_id: Option<Uuid>,
    #[serde(default)]
    person_only: bool,
}

impl From<SubscribeFilter> for Filter {
    fn from(filter: SubscribeFilter) -> Self {
        Self {
            board: filter.board_id,
            person_only: filter.person_only,
        }
    }
}

/// The credential of the upgrade request, in the order of
/// [`auth::require_auth`]: the bearer first, then the session cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Credential {
    Bearer(String),
    Cookie(String),
}

impl Credential {
    /// The credential of `headers`. [`promote_query_token`] has already
    /// put `?access_token=` into the `Authorization` header.
    fn of(headers: &HeaderMap) -> Option<Self> {
        let bearer = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty());
        if let Some(token) = bearer {
            return Some(Self::Bearer(token.to_string()));
        }
        crate::session_cookie::token(headers)
            .filter(|token| crate::local_auth::is_session_token(token))
            .map(Self::Cookie)
    }

    /// Whether the credential is still valid: the session is not ended,
    /// the key is not revoked, the token is not expired.
    async fn is_valid(&self, state: &AppState) -> bool {
        let result = match self {
            Self::Cookie(token) => crate::login::authenticate_session(state, token)
                .await
                .map(drop),
            Self::Bearer(token) if crate::service_accounts::auth::is_api_key(token) => {
                crate::service_accounts::auth::authenticate_api_key(state, token)
                    .await
                    .map(drop)
            }
            Self::Bearer(token) if crate::local_auth::is_session_token(token) => {
                crate::login::authenticate_session(state, token)
                    .await
                    .map(drop)
            }
            Self::Bearer(token) => state
                .auth
                .verify(token)
                .await
                .map(drop)
                .map_err(crate::error::ApiError::from),
        };
        still_valid(&result)
    }
}

/// Whether a check of a credential keeps the socket open. A fault of the
/// server (a 5xx) is not a refusal: the next check decides. Pure.
fn still_valid(result: &Result<(), crate::error::ApiError>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => error.status.is_server_error(),
    }
}

/// `GET /ws/events`: upgrade the (already authenticated, tenant-resolved)
/// request and serve the connection until the client disconnects.
async fn ws_events(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Extension(tenant): Extension<TenantContext>,
    Extension(auth): Extension<AuthContext>,
    Extension(hub): Extension<EventHub>,
) -> Response {
    let rx = hub.subscribe(&tenant.slug);
    let credential = Credential::of(&headers);
    ws.on_upgrade(move |socket| {
        handle_socket(
            socket,
            Session {
                state,
                credential,
                tenant: tenant.slug,
                user: auth.user_id,
            },
            rx,
        )
    })
}

/// Who a socket serves, and how to check it again.
struct Session {
    state: AppState,
    credential: Option<Credential>,
    tenant: String,
    user: Uuid,
}

/// Forward this tenant's events to one socket until it closes. The task
/// owns nothing but the socket and its broadcast receiver, so ending it IS
/// the cleanup (no leaked tasks).
async fn handle_socket(
    mut socket: WebSocket,
    session: Session,
    mut rx: broadcast::Receiver<Arc<BroadcastEvent>>,
) {
    let Session {
        state,
        credential,
        tenant,
        user,
    } = session;
    let mut filter = Filter::default();
    let mut recheck = tokio::time::interval_at(
        tokio::time::Instant::now() + SESSION_RECHECK,
        SESSION_RECHECK,
    );
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) => {
                    if !event.passes(filter, user) {
                        continue;
                    }
                    if socket
                        .send(Message::Text(event.message.clone()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Best-effort: the client missed `skipped` events. The
                    // frame tells it to read again (module docs).
                    tracing::debug!(tenant, skipped, "event socket lagged; events dropped");
                    if socket
                        .send(Message::Text(Utf8Bytes::from_static(LAGGED_FRAME)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = recheck.tick() => {
                let valid = match &credential {
                    Some(credential) => credential.is_valid(&state).await,
                    None => false,
                };
                if !valid {
                    tracing::debug!(tenant, "event socket closed: the credential is not valid");
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: CLOSE_SESSION_ENDED,
                            reason: Utf8Bytes::from_static("The session ended."),
                        })))
                        .await;
                    break;
                }
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(parsed) = serde_json::from_str::<ClientMessage>(&text)
                        && let Some(subscribe) = parsed.subscribe
                    {
                        filter = subscribe.into();
                    }
                }
                // Ping/pong are answered by the protocol layer while we
                // keep reading; binary frames are ignored.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The LISTEN side
// ---------------------------------------------------------------------------

/// Parse one NOTIFY payload (`kairos_db::events` shape): a JSON array of
/// events, or one event (an older server, module docs). An element that is
/// not an event is dropped; the others go.
fn parse_payload(payload: &str) -> Vec<BroadcastEvent> {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(serde_json::Value::Array(events)) => {
            events.into_iter().filter_map(parse_event).collect()
        }
        Ok(event) => parse_event(event).into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse one event: pull out the routing fields and strip `tenant`,
/// `recipients` and `blocks_boards`, so that the forwarded message is
/// exactly the S-0005 client shape.
fn parse_event(mut value: serde_json::Value) -> Option<BroadcastEvent> {
    let object = value.as_object_mut()?;
    let tenant = match object.remove("tenant") {
        Some(serde_json::Value::String(tenant)) => tenant,
        _ => return None,
    };
    // KAIROS-T-0381: an event of a person. A recipient that is not a UUID
    // drops the event: it must not go to each socket of the tenant.
    let uuid = |value: &serde_json::Value| value.as_str().and_then(|s| Uuid::parse_str(s).ok());
    let recipients = match (object.remove("recipients"), object.remove("recipient")) {
        (None, None) => None,
        (Some(serde_json::Value::Array(list)), None) => {
            Some(list.iter().map(uuid).collect::<Option<Vec<Uuid>>>()?)
        }
        (None, Some(one)) => Some(vec![uuid(&one)?]),
        _ => return None,
    };
    let board_id = object
        .get("board_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    // COLLIERY-T-0233: absent on most events. An entry that is not a
    // UUID is dropped, the event is still delivered to its own board.
    let blocks_boards = match object.remove("blocks_boards") {
        Some(serde_json::Value::Array(boards)) => boards
            .iter()
            .filter_map(|v| v.as_str())
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect(),
        _ => Vec::new(),
    };
    Some(BroadcastEvent {
        tenant,
        board_id,
        blocks_boards,
        recipients,
        message: value.to_string().into(),
    })
}

/// Start the per-process LISTEN task: connect, `LISTEN kairos_events`,
/// forward notifications into the hub, and reconnect with exponential
/// backoff when the connection drops. Requires a Tokio runtime; without
/// one (sync router construction in unit tests) the channel simply stays
/// silent.
fn spawn_listener(database_url: String, hub: EventHub) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no tokio runtime at router construction; /ws/events listener not started");
        return;
    };
    handle.spawn(async move {
        let mut backoff = BACKOFF_INITIAL;
        loop {
            match run_listener(&database_url, &hub, &mut backoff).await {
                Ok(()) => tracing::warn!("event LISTEN connection closed"),
                Err(error) => tracing::warn!(%error, "event LISTEN connection failed"),
            }
            let reconnects = hub.reconnects.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                reconnects,
                backoff_ms = backoff.as_millis() as u64,
                "event listener reconnecting"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });
}

/// One LISTEN connection lifetime: returns when the connection ends
/// (`Ok`) or errors. Resets `backoff` once the `LISTEN` is established.
async fn run_listener(
    database_url: &str,
    hub: &EventHub,
    backoff: &mut Duration,
) -> Result<(), tokio_postgres::Error> {
    let (client, mut connection) =
        tokio_postgres::connect(database_url, kairos_db::tls::MakeRustlsConnect::new()).await?;

    // `client` executes the LISTEN while `connection` is polled for it —
    // both live in this task, multiplexed by the select below. After the
    // LISTEN completes only notifications remain.
    let listen = client.batch_execute("LISTEN kairos_events");
    tokio::pin!(listen);
    let mut listening = false;

    loop {
        tokio::select! {
            result = listen.as_mut(), if !listening => {
                result?;
                listening = true;
                *backoff = BACKOFF_INITIAL;
                tracing::info!("event listener attached (LISTEN kairos_events)");
            }
            message = std::future::poll_fn(|cx| connection.poll_message(cx)) => {
                match message {
                    Some(Ok(tokio_postgres::AsyncMessage::Notification(n)))
                        if n.channel() == EVENT_CHANNEL =>
                    {
                        let events = parse_payload(n.payload());
                        if events.is_empty() {
                            tracing::warn!(
                                payload = n.payload(),
                                "unparseable kairos_events payload dropped"
                            );
                        }
                        for event in events {
                            hub.publish(event);
                        }
                    }
                    Some(Ok(_)) => {} // other channels / notices
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()), // connection closed
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The emitter (KAIROS-T-0632)
// ---------------------------------------------------------------------------

/// Start the emitter of this process (module docs, "The emitter"). It runs
/// next to the LISTEN task and needs a Tokio runtime as that task does.
/// Each pass runs on a blocking thread with the one connection of the
/// emitter. A failed pass drops the connection and waits with the backoff;
/// its rows stay unsent, and the next pass, or a different server, sends
/// them.
fn spawn_emitter(database_url: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no tokio runtime at router construction; the event emitter not started");
        return;
    };
    handle.spawn(async move {
        let mut conn: Option<diesel::PgConnection> = None;
        let mut pruned: Option<std::time::Instant> = None;
        let mut backoff = BACKOFF_INITIAL;
        loop {
            let prune = pruned.is_none_or(|at| at.elapsed() >= PRUNE_EVERY);
            let url = database_url.clone();
            let taken = conn.take();
            let pass =
                tokio::task::spawn_blocking(move || emit_pass(&url, taken, prune)).await;
            match pass {
                Ok((kept, Ok(full))) => {
                    conn = kept;
                    backoff = BACKOFF_INITIAL;
                    if prune {
                        pruned = Some(std::time::Instant::now());
                    }
                    // A full batch: more rows wait, so the next pass runs now.
                    if !full {
                        tokio::time::sleep(EMIT_POLL).await;
                    }
                }
                Ok((_, Err(error))) => {
                    tracing::warn!(%error, backoff_ms = backoff.as_millis() as u64, "event emitter failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
                Err(error) => {
                    tracing::warn!(%error, "event emitter pass stopped");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    });
}

/// One pass of the emitter: connect when there is no connection, send the
/// unsent rows, and prune when `prune`. Returns the connection to keep
/// (none after an error) and whether the batch was full.
fn emit_pass(
    database_url: &str,
    conn: Option<diesel::PgConnection>,
    prune: bool,
) -> (Option<diesel::PgConnection>, Result<bool, String>) {
    let mut conn = match conn {
        Some(conn) => conn,
        None => match kairos_db::establish_migration_connection(database_url) {
            Ok(conn) => conn,
            Err(error) => return (None, Err(error.to_string())),
        },
    };
    let result = (|| {
        let mut full = false;
        if kairos_db::events::outbox_pending(&mut conn)? {
            let sent = kairos_db::events::send_outbox(&mut conn, kairos_db::events::SEND_BATCH)?;
            if sent.dropped > 0 {
                tracing::warn!(
                    dropped = sent.dropped,
                    "event emitter dropped events it cannot send"
                );
            }
            tracing::debug!(
                rows = sent.rows,
                notifies = sent.notifies,
                "event emitter pass"
            );
            full = sent.rows as i64 >= kairos_db::events::SEND_BATCH;
        }
        if prune {
            kairos_db::events::prune_outbox(&mut conn)?;
        }
        Ok::<bool, diesel::result::Error>(full)
    })();
    match result {
        Ok(full) => (Some(conn), Ok(full)),
        Err(error) => (None, Err(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one event of a payload that holds one.
    fn one(payload: &str) -> Option<BroadcastEvent> {
        let mut events = parse_payload(payload);
        assert!(events.len() <= 1, "{payload}");
        events.pop()
    }

    /// KAIROS-T-0632: a payload of the emitter is an array. Each element
    /// is one event; an element that is not an event is dropped, and the
    /// others go.
    #[test]
    fn a_batch_gives_each_of_its_events() {
        let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let events = parse_payload(&format!(
            r#"[{{"tenant":"acme","event":"item_created","entity_type":"task",
                  "short_code":"ACME-T-0001","board_id":null}},
                {{"event":"no tenant"}},
                7,
                {{"tenant":"widgets","recipients":["{alice}","{bob}"],
                  "event":"notification_created","entity_type":"notification",
                  "short_code":"WIDGETS-T-0001","board_id":null}}]"#
        ));
        assert_eq!(events.len(), 2, "two events of four elements");
        assert_eq!(events[0].tenant, "acme");
        assert_eq!(events[0].recipients, None);
        let bell = &events[1];
        assert_eq!(bell.tenant, "widgets");
        assert!(bell.visible_to(alice) && bell.visible_to(bob));
        assert!(!bell.visible_to(carol), "never to a person not in the list");
        let forwarded: serde_json::Value =
            serde_json::from_str(&bell.message).expect("forwarded message is JSON");
        assert!(
            forwarded.get("recipients").is_none(),
            "recipients is stripped"
        );
        assert!(forwarded.get("tenant").is_none());

        assert!(parse_payload("[]").is_empty());
        assert!(parse_payload("not json").is_empty());
        // A list with one bad recipient drops the event.
        assert!(
            one(&format!(
                r#"{{"tenant":"acme","recipients":["{alice}","nobody"],"event":"x"}}"#
            ))
            .is_none()
        );
        // Both fields: the event is dropped.
        assert!(
            one(&format!(
                r#"{{"tenant":"acme","recipients":["{alice}"],"recipient":"{bob}","event":"x"}}"#
            ))
            .is_none()
        );
    }

    #[test]
    fn payloads_parse_and_strip_tenant() {
        let board = Uuid::new_v4();
        let event = one(&format!(
            r#"{{"tenant":"acme","event":"item_created","entity_type":"task",
                "short_code":"ACME-T-0001","board_id":"{board}",
                "actor":"7f9c24e5-2b12-4d6a-8f4e-0e1d2c3b4a59",
                "occurred_at":"2026-07-10T12:00:00Z"}}"#
        ))
        .expect("payload parses");
        assert_eq!(event.tenant, "acme");
        assert_eq!(event.board_id, Some(board));
        let forwarded: serde_json::Value =
            serde_json::from_str(&event.message).expect("forwarded message is JSON");
        assert!(forwarded.get("tenant").is_none(), "tenant is stripped");
        assert_eq!(forwarded["event"], "item_created");
        assert_eq!(forwarded["short_code"], "ACME-T-0001");
    }

    /// COLLIERY-T-0233: `blocks_boards` routes the event to the boards
    /// across the item's `blocks` edges, and never reaches a client.
    #[test]
    fn blocks_boards_route_and_are_stripped() {
        let (own, across, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let event = one(&format!(
            r#"{{"tenant":"acme","event":"item_transitioned","entity_type":"task",
                "short_code":"ACME-T-0001","board_id":"{own}",
                "blocks_boards":["{across}","not-a-uuid"],
                "actor":"7f9c24e5-2b12-4d6a-8f4e-0e1d2c3b4a59",
                "occurred_at":"2026-07-10T12:00:00Z"}}"#
        ))
        .expect("payload parses");
        assert_eq!(event.blocks_boards, vec![across]);
        assert!(event.concerns_board(own), "the board of the item");
        assert!(event.concerns_board(across), "a board across a blocks edge");
        assert!(!event.concerns_board(other), "an unrelated board");
        let forwarded: serde_json::Value =
            serde_json::from_str(&event.message).expect("forwarded message is JSON");
        assert!(
            forwarded.get("blocks_boards").is_none(),
            "routing metadata is stripped"
        );
        assert_eq!(forwarded["board_id"], own.to_string());
    }

    #[test]
    fn off_board_and_malformed_payloads() {
        let event = one(
            r#"{"tenant":"acme","event":"item_updated","entity_type":"document",
                "short_code":"ACME-D-0001","board_id":null,
                "actor":"7f9c24e5-2b12-4d6a-8f4e-0e1d2c3b4a59",
                "occurred_at":"2026-07-10T12:00:00Z"}"#,
        )
        .expect("off-board payload parses");
        assert_eq!(event.board_id, None);
        assert!(event.blocks_boards.is_empty());

        assert!(one("not json").is_none());
        assert!(
            one(r#"{"event":"item_created"}"#).is_none(),
            "no tenant tag"
        );
    }

    /// KAIROS-T-0381: an event of a person goes only to the socket of
    /// that person, and the routing field never reaches a client.
    #[test]
    fn an_event_of_a_person_is_for_that_person_only() {
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        let event = one(&format!(
            r#"{{"tenant":"acme","recipient":"{alice}","event":"notification_created",
                "entity_type":"notification","short_code":"ACME-T-0001","board_id":null,
                "actor":"{bob}","occurred_at":"2026-07-10T12:00:00Z"}}"#
        ))
        .expect("payload parses");
        assert_eq!(event.recipients, Some(vec![alice]));
        assert!(event.visible_to(alice));
        assert!(!event.visible_to(bob), "never to a different person");
        assert!(
            !event.concerns_board(Uuid::new_v4()),
            "no board filter gets it"
        );
        let forwarded: serde_json::Value =
            serde_json::from_str(&event.message).expect("forwarded message is JSON");
        assert!(
            forwarded.get("recipient").is_none(),
            "recipient is stripped"
        );
        assert_eq!(forwarded["event"], "notification_created");

        // A tenant event is for each socket of the tenant.
        let tenant_event = one(
            r#"{"tenant":"acme","event":"item_updated","entity_type":"task",
                "short_code":"ACME-T-0001","board_id":null,
                "actor":"7f9c24e5-2b12-4d6a-8f4e-0e1d2c3b4a59",
                "occurred_at":"2026-07-10T12:00:00Z"}"#,
        )
        .expect("payload parses");
        assert!(tenant_event.visible_to(bob));

        // A recipient that is not a UUID drops the event.
        assert!(
            one(r#"{"tenant":"acme","recipient":"nobody","event":"notification_created"}"#)
                .is_none()
        );
        assert!(one(r#"{"tenant":"acme","recipient":7,"event":"notification_created"}"#).is_none());
    }

    /// KAIROS-T-0621: `person_only` passes only the events of the person.
    /// A board filter passes the events of its board and of the person.
    #[test]
    fn the_filters_of_a_socket() {
        let (alice, bob, board, other) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let of_alice = one(&format!(
            r#"{{"tenant":"acme","recipient":"{alice}","event":"notification_created",
                "entity_type":"notification","short_code":"ACME-T-0001","board_id":null}}"#
        ))
        .expect("payload parses");
        let on_board = one(&format!(
            r#"{{"tenant":"acme","event":"item_updated","entity_type":"task",
                "short_code":"ACME-T-0001","board_id":"{board}"}}"#
        ))
        .expect("payload parses");
        let person = Filter {
            board: None,
            person_only: true,
        };
        let to_board = Filter {
            board: Some(board),
            person_only: false,
        };
        let to_other = Filter {
            board: Some(other),
            person_only: false,
        };

        assert!(of_alice.passes(person, alice));
        assert!(!on_board.passes(person, alice), "no event of the tenant");
        assert!(!of_alice.passes(person, bob), "never to a different person");

        assert!(on_board.passes(to_board, alice));
        assert!(
            of_alice.passes(to_board, alice),
            "the bell shares the socket"
        );
        assert!(!of_alice.passes(to_board, bob));
        assert!(!on_board.passes(to_other, alice));

        assert!(on_board.passes(Filter::default(), bob), "no filter");
        assert!(of_alice.passes(Filter::default(), alice));
        assert!(!of_alice.passes(Filter::default(), bob));

        // `person_only` wins over a board.
        let both = Filter {
            board: Some(board),
            person_only: true,
        };
        assert!(!on_board.passes(both, alice));
        assert!(of_alice.passes(both, alice));

        let parsed: ClientMessage =
            serde_json::from_str(r#"{"subscribe":{"person_only":true}}"#).expect("message");
        assert_eq!(Filter::from(parsed.subscribe.expect("subscribe")), person);
        let parsed: ClientMessage =
            serde_json::from_str(&format!(r#"{{"subscribe":{{"board_id":"{board}"}}}}"#))
                .expect("message");
        assert_eq!(Filter::from(parsed.subscribe.expect("subscribe")), to_board);
    }

    fn event_of(tenant: &str, code: &str) -> BroadcastEvent {
        one(&format!(
            r#"{{"tenant":"{tenant}","event":"item_updated","entity_type":"task",
                "short_code":"{code}","board_id":null}}"#
        ))
        .expect("payload parses")
    }

    /// KAIROS-T-0621: each tenant has its own channel. A tenant with no
    /// socket loses its channel. A slow receiver gets `Lagged`.
    #[tokio::test]
    async fn a_channel_for_each_tenant() {
        let hub = EventHub::with_capacity(2);
        let mut acme = hub.subscribe("acme");
        let mut widgets = hub.subscribe("widgets");
        hub.publish(event_of("acme", "ACME-T-0001"));
        hub.publish(event_of("ghost", "GHOST-T-0001"));
        assert_eq!(
            acme.recv().await.expect("event").message.as_str(),
            one(
                r#"{"tenant":"acme","event":"item_updated","entity_type":"task",
                    "short_code":"ACME-T-0001","board_id":null}"#
            )
            .expect("payload parses")
            .message
            .as_str()
        );
        assert!(
            matches!(
                widgets.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "widgets gets no event of acme"
        );
        assert!(!hub.channels.lock().expect("lock").contains_key("ghost"));

        for n in 0..4 {
            hub.publish(event_of("acme", &format!("ACME-T-000{n}")));
        }
        assert!(matches!(
            acme.recv().await,
            Err(broadcast::error::RecvError::Lagged(2))
        ));

        drop(widgets);
        hub.publish(event_of("widgets", "WIDGETS-T-0001"));
        assert!(
            !hub.channels.lock().expect("lock").contains_key("widgets"),
            "the channel goes with its last socket"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(LAGGED_FRAME).expect("JSON"),
            serde_json::json!({ "event": "lagged" })
        );
    }

    /// KAIROS-T-0621: the socket checks again the credential of its
    /// upgrade: the bearer first, then the session cookie.
    #[test]
    fn the_credential_of_the_upgrade() {
        let mut headers = HeaderMap::new();
        assert_eq!(Credential::of(&headers), None);
        let session = format!("{}abc", crate::local_auth::SESSION_PREFIX);
        let cookie = format!("{}={session}", crate::session_cookie::NAME);
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(&cookie).expect("cookie"),
        );
        assert_eq!(
            Credential::of(&headers),
            Some(Credential::Cookie(session.clone()))
        );
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer a.b.c"));
        assert_eq!(
            Credential::of(&headers),
            Some(Credential::Bearer("a.b.c".to_string()))
        );
    }

    /// KAIROS-T-0621: a refused credential closes the socket. A fault of
    /// the server does not.
    #[test]
    fn a_refusal_closes_and_a_fault_does_not() {
        use crate::error::ApiError;
        assert!(still_valid(&Ok(())));
        assert!(!still_valid(&Err(ApiError::unauthorized("ended"))));
        assert!(still_valid(&Err(ApiError::internal(
            "the database is down"
        ))));
    }

    #[test]
    fn access_token_query_extraction() {
        assert_eq!(
            query_access_token("access_token=abc.def.ghi"),
            Some("abc.def.ghi")
        );
        assert_eq!(query_access_token("foo=1&access_token=t&bar=2"), Some("t"));
        assert_eq!(query_access_token("access_token="), None);
        assert_eq!(query_access_token("other=1"), None);
    }
}
