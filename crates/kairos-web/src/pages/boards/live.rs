//! `/ws/events` live updates (KAIROS-T-0040, contract per
//! docs/src/content/docs/reference/events.md / KAIROS-A-0005 §5).
//!
//! The server pushes thin change notifications; the view reacts by
//! reading through REST. The board view reads the one card that an event
//! names, and puts it in its model (KAIROS-T-0615); the other views read
//! again what they show. Browser `WebSocket` clients cannot set request
//! headers, so the
//! token travels via the ONE documented fallback, the `?access_token=`
//! query parameter (`kairos-server::ws::promote_query_token`).
//!
//! # One socket for each tab (KAIROS-T-0621)
//!
//! The tab has one hub and one socket. The bell and each live view
//! register a [`Want`] with the hub. The hub sends the server one filter
//! for all registrations ([`Mode::of`]):
//! - the bell alone: `{"subscribe": {"person_only": true}}`;
//! - one board (and the bell): `{"subscribe": {"board_id": …}}`. The
//!   server also sends the events of the person on that filter;
//! - each other case: `{"subscribe": {}}`, the whole tenant stream.
//!
//! The hub parses each frame once and gives the event to each
//! registration that wants it ([`routes`]). Every kind is treated alike,
//! `item_moved` (KAIROS-I-0012) included: the server emits that one per
//! side of a board move, so both boards' subscribers read the card off
//! their own half.
//!
//! - On close: reconnect with exponential backoff (1s → 15s cap) and
//!   ±50 % jitter ([`backoff_ms`]). Thus the tabs do not all reconnect in
//!   the same second after a restart of the server.
//! - On each RE-open, and on the frame `{"event":"lagged"}` from the
//!   server: each registration gets `None` once. That is the reconcile for
//!   the events that the tab missed.
//! - When the last registration drops: close the socket and stop the
//!   backoff timer chain.
//!
//! The events of a person (`entity_type` `notification`, KAIROS-T-0381) go
//! only to [`subscribe_person_events`] (the bell of the shell). The views
//! of the items do not get them: they change no item.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::set_timeout;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{MessageEvent, WebSocket};

use super::data::ThinEvent;
use crate::auth::Auth;

/// Reconnect backoff bounds (milliseconds).
const BACKOFF_INITIAL_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 15_000;

/// The `event` of the frame that says that the socket skipped events.
const LAGGED: &str = "lagged";

/// What one registration wants from the stream. Pure, host-tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    /// The events of the items of one board.
    Board(String),
    /// The events of the items of the whole tenant.
    Items,
    /// The events of the person (the notifications).
    Person,
}

/// The filter that the socket sends to the server. Pure, host-tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Only the events of the person.
    Person,
    /// The events of one board, and of the person.
    Board(String),
    /// Each event of the tenant.
    All,
}

impl Mode {
    /// The filter for these registrations, or `None` when the tab needs no
    /// socket.
    pub(crate) fn of<'a>(wants: impl IntoIterator<Item = &'a Want>) -> Option<Self> {
        let mut board: Option<&str> = None;
        let mut person = false;
        let mut all = false;
        for want in wants {
            match want {
                Want::Items => all = true,
                Want::Person => person = true,
                Want::Board(id) => match board {
                    None => board = Some(id),
                    Some(seen) if seen == id => {}
                    Some(_) => all = true,
                },
            }
        }
        if all {
            Some(Self::All)
        } else if let Some(board) = board {
            Some(Self::Board(board.to_string()))
        } else if person {
            Some(Self::Person)
        } else {
            None
        }
    }

    /// The subscribe message of this filter.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Person => serde_json::json!({ "subscribe": { "person_only": true } }),
            Self::Board(board_id) => serde_json::json!({ "subscribe": { "board_id": board_id } }),
            Self::All => serde_json::json!({ "subscribe": {} }),
        }
        .to_string()
    }
}

/// Whether a registration that wants `want` gets `event`, while the
/// socket has the filter `mode`. With the filter of its board, a board
/// view gets each event of the items: the server also sends the events
/// across a `blocks` edge (COLLIERY-T-0233). With the whole stream, it
/// gets only the events of its board. Pure, host-tested.
pub(crate) fn routes(want: &Want, mode: Option<&Mode>, event: &ThinEvent) -> bool {
    let person = is_person_event(event);
    match want {
        Want::Person => person,
        Want::Items => !person,
        Want::Board(id) => {
            !person
                && (matches!(mode, Some(Mode::Board(filter)) if filter == id)
                    || event.board_id.as_deref() == Some(id.as_str()))
        }
    }
}

/// The reconnect delay after `attempt` failed connections. `unit` is a
/// random number in `[0, 1)`: the delay is 50 % to 150 % of the doubled
/// base. Pure, host-tested.
pub(crate) fn backoff_ms(attempt: u32, unit: f64) -> u64 {
    let base = (BACKOFF_INITIAL_MS << attempt.min(4)).min(BACKOFF_MAX_MS);
    let factor = 0.5 + unit.clamp(0.0, 1.0);
    // The base is at most 15 000: the cast loses nothing.
    (base as f64 * factor).round() as u64
}

/// The hook of a registration: called with the event when one arrived,
/// `None` for a reconcile.
type OnEvent = Rc<dyn Fn(Option<&ThinEvent>)>;

struct Registration {
    id: u64,
    want: Want,
    on_event: OnEvent,
}

/// The wasm closures of the socket. The hub makes them once and keeps
/// them for the life of the tab. Thus a handler never drops itself.
struct Handlers {
    on_open: Closure<dyn FnMut()>,
    on_message: Closure<dyn FnMut(MessageEvent)>,
    on_close: Closure<dyn FnMut()>,
}

/// The one socket of the tab, and the registrations that share it.
#[derive(Default)]
struct Hub {
    /// The auth of the last registration.
    auth: Cell<Option<Auth>>,
    registrations: RefCell<Vec<Registration>>,
    next_id: Cell<u64>,
    socket: RefCell<Option<WebSocket>>,
    /// The filter that the open socket sent last.
    sent: RefCell<Option<Mode>>,
    /// Consecutive failed/closed connections (drives the backoff).
    attempts: Cell<u32>,
    /// Whether a connection opened since the hub had a registration.
    /// A re-open reconciles; the first open does not: the views load.
    ever_opened: Cell<bool>,
    /// A timer of an older generation does nothing: the socket closed
    /// for good after it started.
    generation: Cell<u64>,
    reconnect_pending: Cell<bool>,
    handlers: RefCell<Option<Handlers>>,
}

thread_local! {
    static HUB: Hub = Hub::default();
}

/// Dropping this removes the registration from the hub. The last one
/// closes the socket. Hold it in the component
/// (`on_cleanup(move || drop(guard))`).
pub struct LiveBoardGuard {
    id: u64,
}

impl Drop for LiveBoardGuard {
    fn drop(&mut self) {
        let id = self.id;
        let _ = HUB.try_with(|hub| {
            hub.registrations.borrow_mut().retain(|r| r.id != id);
            hub.sync();
        });
    }
}

/// Subscribe the board view to `/ws/events`, filtered to `board_id`.
/// `on_event` runs on every board event with the event, and on every
/// reconcile with `None` (KAIROS-T-0615: the view reads the card of the
/// event, or the board).
pub fn subscribe_board_events(
    auth: Auth,
    board_id: String,
    on_event: impl Fn(Option<&ThinEvent>) + 'static,
) -> LiveBoardGuard {
    register(auth, Want::Board(board_id), on_event)
}

/// Subscribe to the WHOLE tenant stream (no board filter): the graph
/// view's mode (KAIROS-T-0090). `on_event` receives the event's
/// short_code when present, `None` on reconciles.
///
/// `board_configured` (KAIROS-T-0574) does not reach `on_event`: its
/// `short_code` is the slug of a board, which a view of the whole stream
/// could take for the slug of a repository. Those views show no board
/// configuration.
pub fn subscribe_all_events(
    auth: Auth,
    on_event: impl Fn(Option<&str>) + 'static,
) -> LiveBoardGuard {
    register(auth, Want::Items, move |event| {
        if !is_board_configuration(event) {
            on_event(event_code(event));
        }
    })
}

/// Subscribe to the WHOLE tenant stream with each event, kind included:
/// the item page (KAIROS-T-0578) reads by the kind what changed.
/// `board_configured` does not reach `on_event`, as for
/// [`subscribe_all_events`].
pub fn subscribe_item_events(
    auth: Auth,
    on_event: impl Fn(Option<&ThinEvent>) + 'static,
) -> LiveBoardGuard {
    register(auth, Want::Items, move |event| {
        if !is_board_configuration(event) {
            on_event(event);
        }
    })
}

/// Subscribe to the events of the signed-in person (KAIROS-T-0383):
/// `notification_created` and `notifications_read`. The server sends them
/// to the sockets of that person only. `on_event` runs on each one and on
/// each reconcile.
pub fn subscribe_person_events(auth: Auth, on_event: impl Fn() + 'static) -> LiveBoardGuard {
    register(auth, Want::Person, move |_| on_event())
}

/// Whether the event is a change of the configuration of a board. Pure,
/// host-tested.
pub(crate) fn is_board_configuration(event: Option<&ThinEvent>) -> bool {
    event.is_some_and(|event| event.event == super::data::BOARD_CONFIGURED)
}

/// The short code of an event, or `None` for a reconnect and for an event
/// that names no item. Pure, host-tested.
pub(crate) fn event_code(event: Option<&ThinEvent>) -> Option<&str> {
    event
        .map(|event| event.short_code.as_str())
        .filter(|code| !code.is_empty())
}

/// Whether an event is an event of a person (a notification). Pure,
/// host-tested.
pub(crate) fn is_person_event(event: &ThinEvent) -> bool {
    event.entity_type == "notification"
}

fn register(
    auth: Auth,
    want: Want,
    on_event: impl Fn(Option<&ThinEvent>) + 'static,
) -> LiveBoardGuard {
    HUB.with(|hub| {
        hub.auth.set(Some(auth));
        let id = hub.next_id.get();
        hub.next_id.set(id + 1);
        hub.registrations.borrow_mut().push(Registration {
            id,
            want,
            on_event: Rc::new(on_event),
        });
        hub.sync();
        LiveBoardGuard { id }
    })
}

/// The `ws(s)://…/ws/events?access_token=…` URL for the current origin.
/// Browser WS clients cannot set the Authorization header — the query
/// parameter is the documented fallback (docs/src/content/docs/reference/events.md).
/// A cookie session (KAIROS-T-0327) has no token in memory: the handshake
/// then carries the HttpOnly session cookie, and the URL has no parameter.
fn events_url(token: &str) -> Option<String> {
    let location = web_sys::window()?.location();
    let protocol = location.protocol().ok()?;
    let host = location.host().ok()?;
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    Some(if token.is_empty() {
        format!("{scheme}://{host}/ws/events")
    } else {
        format!("{scheme}://{host}/ws/events?access_token={token}")
    })
}

impl Hub {
    fn mode(&self) -> Option<Mode> {
        Mode::of(self.registrations.borrow().iter().map(|r| &r.want))
    }

    /// Bring the socket in line with the registrations: open it, send a
    /// new filter, or close it.
    fn sync(&self) {
        let Some(mode) = self.mode() else {
            self.shut();
            return;
        };
        let open = self
            .socket
            .borrow()
            .as_ref()
            .map(|socket| socket.ready_state() == WebSocket::OPEN);
        match open {
            Some(true) => {
                if self.sent.borrow().as_ref() != Some(&mode) {
                    self.send(mode);
                }
            }
            // Connecting: the open sends the filter of that moment.
            Some(false) => {}
            None => {
                if !self.reconnect_pending.get() {
                    self.connect();
                }
            }
        }
    }

    /// Close the socket for good: no registration is left.
    fn shut(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.reconnect_pending.set(false);
        if let Some(socket) = self.socket.borrow_mut().take() {
            // Detach the handlers first: the close then schedules nothing.
            socket.set_onopen(None);
            socket.set_onmessage(None);
            socket.set_onclose(None);
            let _ = socket.close();
        }
        self.sent.replace(None);
        self.attempts.set(0);
        self.ever_opened.set(false);
    }

    fn send(&self, mode: Mode) {
        if let Some(socket) = self.socket.borrow().as_ref() {
            let _ = socket.send_with_str(&mode.message());
        }
        self.sent.replace(Some(mode));
    }

    /// Open one connection attempt.
    fn connect(&self) {
        if self.mode().is_none() || self.socket.borrow().is_some() {
            return;
        }
        // No session (mid-expiry): retry later; the shell guard owns re-auth.
        let url = self
            .auth
            .get()
            .and_then(|auth| auth.token())
            .and_then(|token| events_url(&token));
        let Some(socket) = url.and_then(|url| WebSocket::new(&url).ok()) else {
            self.schedule_reconnect();
            return;
        };
        self.attach(&socket);
        self.socket.replace(Some(socket));
    }

    fn attach(&self, socket: &WebSocket) {
        let mut handlers = self.handlers.borrow_mut();
        let handlers = handlers.get_or_insert_with(|| Handlers {
            on_open: Closure::wrap(Box::new(|| HUB.with(Hub::opened)) as Box<dyn FnMut()>),
            on_message: Closure::wrap(Box::new(|event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    HUB.with(|hub| hub.message(&text));
                }
            }) as Box<dyn FnMut(MessageEvent)>),
            on_close: Closure::wrap(Box::new(|| HUB.with(Hub::closed)) as Box<dyn FnMut()>),
        });
        socket.set_onopen(Some(handlers.on_open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(handlers.on_message.as_ref().unchecked_ref()));
        // An errored socket also fires `close`; one handler covers both.
        socket.set_onclose(Some(handlers.on_close.as_ref().unchecked_ref()));
    }

    fn opened(&self) {
        self.attempts.set(0);
        let Some(mode) = self.mode() else {
            return;
        };
        self.send(mode);
        if self.ever_opened.replace(true) {
            // Reconnect: silently reconcile whatever was missed.
            self.dispatch(None);
        }
    }

    /// One frame: one parse, then each registration that wants it.
    fn message(&self, text: &str) {
        let Ok(event) = serde_json::from_str::<ThinEvent>(text) else {
            return;
        };
        if event.event == LAGGED {
            self.dispatch(None);
            return;
        }
        let mode = self.sent.borrow().clone();
        let targets: Vec<OnEvent> = self
            .registrations
            .borrow()
            .iter()
            .filter(|r| routes(&r.want, mode.as_ref(), &event))
            .map(|r| Rc::clone(&r.on_event))
            .collect();
        // No borrow is held here: a hook can drop or add a registration.
        for on_event in targets {
            on_event(Some(&event));
        }
    }

    /// Each registration gets `None`: read again what it shows.
    fn dispatch(&self, event: Option<&ThinEvent>) {
        let targets: Vec<OnEvent> = self
            .registrations
            .borrow()
            .iter()
            .map(|r| Rc::clone(&r.on_event))
            .collect();
        for on_event in targets {
            on_event(event);
        }
    }

    fn closed(&self) {
        self.socket.borrow_mut().take();
        self.sent.replace(None);
        self.schedule_reconnect();
    }

    /// Queue the next connection attempt with jittered exponential backoff.
    fn schedule_reconnect(&self) {
        if self.mode().is_none() {
            return;
        }
        let attempt = self.attempts.get();
        self.attempts.set(attempt.saturating_add(1));
        let delay = backoff_ms(attempt, js_sys::Math::random());
        let generation = self.generation.get();
        self.reconnect_pending.set(true);
        set_timeout(
            move || {
                HUB.with(|hub| {
                    if hub.generation.get() != generation {
                        return;
                    }
                    hub.reconnect_pending.set(false);
                    hub.connect();
                });
            },
            Duration::from_millis(delay),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, ThinEvent, Want, backoff_ms, routes};

    fn event(text: &str) -> ThinEvent {
        serde_json::from_str(text).expect("event decodes")
    }

    /// KAIROS-T-0383: the bell gets the events of the person, and the views
    /// of the items do not.
    #[test]
    fn a_notification_event_is_an_event_of_the_person() {
        assert!(super::is_person_event(&event(
            r#"{"event":"notification_created","entity_type":"notification","short_code":"A-T-1"}"#
        )));
        assert!(!super::is_person_event(&event(
            r#"{"event":"item_updated","entity_type":"task","short_code":"A-T-1"}"#
        )));
        // KAIROS-T-0621: the frame of a lag decodes, and names no item.
        let lagged = event(r#"{"event":"lagged"}"#);
        assert_eq!(lagged.event, super::LAGGED);
        assert!(!super::is_person_event(&lagged));
    }

    /// KAIROS-T-0615: the views of the whole stream get the short code of
    /// an event, and `None` for a reconnect or an event with no item.
    #[test]
    fn the_code_of_an_event() {
        let note = event(r#"{"event":"item_updated","short_code":"A-T-1","board_id":"b"}"#);
        assert_eq!(super::event_code(Some(&note)), Some("A-T-1"));
        let read = event(r#"{"event":"notifications_read","short_code":""}"#);
        assert_eq!(super::event_code(Some(&read)), None);
        assert_eq!(super::event_code(None), None);
        // KAIROS-T-0574: the views of the whole stream skip a change of a
        // board configuration, and get each other kind.
        let configured =
            event(r#"{"event":"board_configured","short_code":"demo","board_id":"b"}"#);
        assert!(super::is_board_configuration(Some(&configured)));
        let claim = event(r#"{"event":"claim_changed","short_code":"A-T-1","board_id":"b"}"#);
        assert!(!super::is_board_configuration(Some(&claim)));
        assert!(!super::is_board_configuration(None));
    }

    /// KAIROS-T-0621: one filter for all registrations of the tab.
    #[test]
    fn the_filter_of_the_tab() {
        let board = |id: &str| Want::Board(id.to_string());
        assert_eq!(Mode::of(&[]), None, "no registration, no socket");
        assert_eq!(Mode::of(&[Want::Person]), Some(Mode::Person));
        assert_eq!(
            Mode::of(&[Want::Person, board("b")]),
            Some(Mode::Board("b".to_string())),
            "the bell and one board share the board filter"
        );
        assert_eq!(
            Mode::of(&[board("b"), board("b")]),
            Some(Mode::Board("b".to_string()))
        );
        assert_eq!(Mode::of(&[board("b"), board("c")]), Some(Mode::All));
        assert_eq!(Mode::of(&[Want::Person, Want::Items]), Some(Mode::All));
        assert_eq!(Mode::of(&[board("b"), Want::Items]), Some(Mode::All));

        let message = |mode: Mode| -> serde_json::Value {
            serde_json::from_str(&mode.message()).expect("JSON")
        };
        assert_eq!(
            message(Mode::Person),
            serde_json::json!({ "subscribe": { "person_only": true } })
        );
        assert_eq!(
            message(Mode::Board("b".to_string())),
            serde_json::json!({ "subscribe": { "board_id": "b" } })
        );
        assert_eq!(message(Mode::All), serde_json::json!({ "subscribe": {} }));
    }

    /// KAIROS-T-0621: the hub gives each event to the registrations that
    /// want it.
    #[test]
    fn the_route_of_an_event() {
        let on_b = event(
            r#"{"event":"item_updated","entity_type":"task","short_code":"A-T-1","board_id":"b"}"#,
        );
        let on_c = event(
            r#"{"event":"item_updated","entity_type":"task","short_code":"A-T-2","board_id":"c"}"#,
        );
        let bell = event(
            r#"{"event":"notification_created","entity_type":"notification","short_code":"A-T-1"}"#,
        );
        let b = Want::Board("b".to_string());
        let by_board = Mode::Board("b".to_string());

        assert!(routes(&Want::Person, Some(&by_board), &bell));
        assert!(!routes(&Want::Person, Some(&Mode::All), &on_b));
        assert!(routes(&Want::Items, Some(&Mode::All), &on_b));
        assert!(!routes(&Want::Items, Some(&Mode::All), &bell));

        assert!(routes(&b, Some(&by_board), &on_b));
        assert!(
            routes(&b, Some(&by_board), &on_c),
            "the server sent it across a blocks edge"
        );
        assert!(!routes(&b, Some(&by_board), &bell));
        assert!(routes(&b, Some(&Mode::All), &on_b));
        assert!(
            !routes(&b, Some(&Mode::All), &on_c),
            "the whole stream: only the events of the board"
        );
    }

    /// The backoff doubles from 1s and caps at 15s. KAIROS-T-0621: the
    /// jitter puts each delay between 50 % and 150 % of that base.
    #[test]
    fn backoff_doubles_caps_and_jitters() {
        let delays = |unit: f64| -> Vec<u64> { (0u32..7).map(|n| backoff_ms(n, unit)).collect() };
        assert_eq!(
            delays(0.5),
            vec![1_000, 2_000, 4_000, 8_000, 15_000, 15_000, 15_000]
        );
        assert_eq!(
            delays(0.0),
            vec![500, 1_000, 2_000, 4_000, 7_500, 7_500, 7_500]
        );
        assert_eq!(backoff_ms(0, 1.0), 1_500);
        assert_eq!(backoff_ms(9, 0.999), 22_485);
        assert_eq!(backoff_ms(0, -1.0), 500, "a bad unit is clamped");
    }
}
