//! Websocket connections and event broadcast, as Mattermost's platform hub does it on one node: an event is
//! serialised once, each connection adds its own `seq`; who gets it follows `WebConn.shouldSendEvent`.

// Based on Mattermost server/channels/app/platform/web_hub.go, web_conn.go and channels/wsapi, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State as St};
use axum::http::request::Parts;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::app::{State, new_id, now_ms, token_from};
use crate::model::Session;

#[derive(Default)]
pub struct Hub {
    conns: RwLock<Vec<Arc<Conn>>>,
    next: AtomicU64,
    /// Events whose recipients depend on channel membership go through the app; see [`Hub::publish`].
    app: std::sync::OnceLock<std::sync::Weak<crate::app::App>>,
}

pub struct Conn {
    id: u64,
    pub user_id: String,
    pub connection_id: String,
    token: String,
    /// The session events are filtered with (`WebConn.session`): the connect's, then the one reloaded after an
    /// invalidation or its expiry. `admin`, `guest`, `expires_at` and `teams` are read from it, so that the hot
    /// path is a few atomic loads.
    session: RwLock<Arc<Session>>,
    admin: AtomicBool,
    guest: AtomicBool,
    /// `sessionExpiresAt` (0: never).
    expires_at: AtomicI64,
    /// `InvalidateCache` bumps `generation`; the session is loaded again while `loaded` differs.
    generation: AtomicU64,
    loaded: AtomicU64,
    /// Go's cleared session token: the session could not be loaded again (or was revoked), nothing more is sent.
    dead: AtomicBool,
    teams: RwLock<HashSet<String>>,
    tx: mpsc::Sender<Out>,
    /// Wakes the writer to close the socket: a full queue cannot take `Out::Close`.
    closing: Arc<tokio::sync::Notify>,
    active: AtomicBool,
    inactive_at: AtomicI64,
    /// While no socket serves it: its queue and the events last sent, for a reconnect.
    parked: Mutex<Option<Parked>>,
    presence: RwLock<Presence>,
    /// Connected with `posted_ack=true` (the mobile app): `posted` events it should acknowledge say `should_ack`.
    posted_ack: bool,
}

/// The channel and threads the client shows, once it said (`presence`).
#[derive(Default)]
struct Presence {
    channel: Option<String>,
    rhs_thread_channel: Option<String>,
    thread_view_channel: Option<String>,
}

impl Presence {
    fn away_from(&self, channel_id: &str) -> bool {
        let other = |v: &Option<String>| v.as_deref().is_some_and(|v| v != channel_id);
        other(&self.channel) && other(&self.rhs_thread_channel) && other(&self.thread_view_channel)
    }
}

/// `sendQueueSize`: events a connection may have waiting. A client that lets it fill is closed, as Go's hub does
/// (`webhub.broadcast: cannot send, closing websocket`); it reconnects and loads what it missed. Without the bound
/// one stuck client would hold every event sent to it in memory.
const SEND_QUEUE: usize = 256;
/// `pongWaitTime`: how long a connection lives without a pong.
const PONG_WAIT: Duration = Duration::from_secs(100);
/// `pingInterval`: six tenths of `pongWaitTime`.
const PING_INTERVAL: Duration = Duration::from_secs(60);
const SENT_KEPT: usize = 128;
const INACTIVE_KEPT: Duration = Duration::from_secs(300);
const AUTH_WAIT: Duration = Duration::from_secs(5);
const WRITE_WAIT: Duration = Duration::from_secs(10);

impl Conn {
    fn new(id: u64, session: Arc<Session>, tx: mpsc::Sender<Out>, connection_id: String) -> Conn {
        let c = Conn {
            id,
            user_id: session.user_id.clone(),
            connection_id,
            token: session.token.clone(),
            session: RwLock::new(session.clone()),
            admin: AtomicBool::new(false),
            guest: AtomicBool::new(false),
            expires_at: AtomicI64::new(0),
            generation: AtomicU64::new(0),
            loaded: AtomicU64::new(0),
            dead: AtomicBool::new(false),
            teams: RwLock::new(HashSet::new()),
            tx,
            closing: Arc::new(tokio::sync::Notify::new()),
            active: AtomicBool::new(true),
            inactive_at: AtomicI64::new(0),
            parked: Mutex::new(None),
            presence: RwLock::new(Presence::default()),
            posted_ack: false,
        };
        c.set_session(session);
        c
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn set_presence(&self, data: &Value) {
        let mut p = self.presence.write().unwrap();
        if let Some(c) = data["channel_id"].as_str() {
            p.channel = Some(c.to_string());
        }
        if let Some(t) = data["thread_channel_id"].as_str() {
            if data["is_thread_view"].as_bool() == Some(true) {
                p.thread_view_channel = Some(t.to_string());
            } else {
                p.rhs_thread_channel = Some(t.to_string());
            }
        }
    }

    /// `SetSession` and `SetSessionExpiresAt`, with what the filters read from the session.
    fn set_session(&self, s: Arc<Session>) {
        self.admin.store(s.is_system_admin(), Ordering::Relaxed);
        self.guest.store(s.prop("is_guest") == "true", Ordering::Relaxed);
        self.expires_at.store(s.expires_at, Ordering::Relaxed);
        *self.teams.write().unwrap() = s.team_members.iter().map(|t| t.team_id.clone()).collect();
        *self.session.write().unwrap() = s;
    }

    fn session(&self) -> Arc<Session> {
        self.session.read().unwrap().clone()
    }

    /// Queues a frame for the writer; a full queue closes the connection (Go's `closeAndRemoveConn`).
    fn send(&self, out: Out) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(out)
            && !self.dead.swap(true, Ordering::Relaxed)
        {
            crate::logs::error(
                "webhub.broadcast: cannot send, closing websocket for user",
                &[("user_id", json!(self.user_id)), ("conn_id", json!(self.connection_id))],
            );
            self.closing.notify_one();
        }
    }

    /// Closes the socket; nothing more is sent to it meanwhile.
    fn close(&self) {
        self.dead.store(true, Ordering::Relaxed);
        self.closing.notify_one();
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Whether the session must be loaded again before an event goes out: invalidated (`InvalidateCache`) or past
    /// its expiry (`IsBasicAuthenticated`).
    fn is_stale(&self, now: i64) -> bool {
        let exp = self.expires_at.load(Ordering::Relaxed);
        self.generation.load(Ordering::Acquire) != self.loaded.load(Ordering::Acquire) || (exp > 0 && exp < now)
    }

    /// `IsBasicAuthenticated`: the session as it is now when it is stale. A session that is gone or invalid ends
    /// the deliveries as Go's cleared token does (the socket stays open; its requests fail with the session
    /// error). A server error closes the socket instead of silencing it for good (Go clears the token on any
    /// error): the client reconnects and loads what it missed.
    async fn authenticate(&self, app: &crate::app::App) -> bool {
        if self.is_dead() {
            return false;
        }
        if !self.is_stale(now_ms()) {
            return true;
        }
        let generation = self.generation.load(Ordering::Acquire);
        match app.session(&self.token).await {
            Ok(s) => {
                self.set_session(s);
                self.loaded.store(generation, Ordering::Release);
                true
            }
            Err(e) => {
                self.dead.store(true, Ordering::Relaxed);
                if e.status_code >= 500 {
                    self.closing.notify_one();
                }
                false
            }
        }
    }
}

enum Out {
    /// An event without its `seq`, which the writer appends.
    Event(Arc<str>),
    /// A reply to the client's action: not numbered.
    Reply(String),
}

/// The events last sent, with their `seq`.
#[derive(Default)]
struct Sent(VecDeque<(i64, Arc<str>)>);

impl Sent {
    fn push(&mut self, seq: i64, event: Arc<str>) {
        if self.0.len() == SENT_KEPT {
            self.0.pop_front();
        }
        self.0.push_back((seq, event));
    }

    fn find(&self, seq: i64) -> Option<usize> {
        self.0.iter().position(|(s, _)| *s == seq)
    }

    /// Whether events after the last one kept are missing for a client that wants `seq` next.
    fn lost(&self, seq: i64) -> bool {
        self.0.back().is_some_and(|(s, _)| *s != seq.wrapping_sub(1))
    }
}

struct Parked {
    rx: mpsc::Receiver<Out>,
    sent: Sent,
}

fn frame(event: &str, seq: i64) -> Utf8Bytes {
    let mut s = String::with_capacity(event.len() + 24);
    s.push_str(event);
    s.push_str(&seq.to_string());
    s.push('}');
    Utf8Bytes::from(s)
}

fn hello(user_id: &str, connection_id: &str) -> Arc<str> {
    let mut data = json!({
        "connection_id": connection_id,
        "server_version": format!("{}.{}.{}.false", crate::SERVER_VERSION_FULL, crate::SERVER_VERSION, "0"),
    });
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        data["server_hostname"] = Value::from(name.trim());
    }
    prefix("hello", &data, &Broadcast { user_id: user_id.to_string(), ..Default::default() })
}

/// `model.WebsocketBroadcast`: the part clients see, and how recipients are chosen.
#[derive(Serialize, Default, Clone)]
pub struct Broadcast {
    pub omit_users: Option<BTreeMap<String, bool>>,
    pub user_id: String,
    pub channel_id: String,
    pub team_id: String,
    pub connection_id: String,
    pub omit_connection_id: String,
    /// `ContainsSanitizedData`: non-admins get this data instead.
    #[serde(skip)]
    pub sanitized: Option<Value>,
    /// `ContainsSensitiveData`: admins only.
    #[serde(skip)]
    pub admin_only: bool,
    /// `RequiredPermissions`: only sessions with every one of these system permissions (it takes precedence
    /// over `admin_only`).
    #[serde(skip)]
    pub required_permissions: Vec<&'static str>,
    /// Broadcast hooks (`add_mentions`, `add_followers`): fields added to the data for one recipient.
    #[serde(skip)]
    pub per_user: Option<PerUser>,
    /// A hook that needs the app (`channelMentionsBroadcastHook`): the event's `post` for the user of a connection,
    /// when it is not the shared one.
    #[serde(skip)]
    pub post_hook: Option<PostHook>,
    /// `postedAckBroadcastHook`: which recipients of a `posted` event should acknowledge it.
    #[serde(skip)]
    pub posted_ack: Option<Arc<PostedAck>>,
}

/// The poster, whether the channel is direct, and the users notified of every message in it.
pub struct PostedAck {
    pub poster: String,
    pub direct: bool,
    pub users: HashSet<String>,
}

impl PostedAck {
    /// Whether the user of a connection with `posted_ack=true` should acknowledge: not their own post; mentioned
    /// or following (`extra` has `mentions` or `followers`), in a direct channel, or one of `users`.
    fn should_ack(&self, user_id: &str, extra: Option<&serde_json::Map<String, Value>>) -> bool {
        user_id != self.poster
            && (extra.is_some_and(|e| e.contains_key("mentions") || e.contains_key("followers"))
                || self.direct
                || self.users.contains(user_id))
    }
}

/// Fields to add to an event's data for the user of a connection, if any.
pub type PerUser = Arc<dyn Fn(&str) -> Option<serde_json::Map<String, Value>> + Send + Sync>;

/// The `post` of an event for one recipient (by user id), when it differs from the shared one.
pub type PostHook =
    Arc<dyn Fn(State, String) -> futures_util::future::BoxFuture<'static, Option<String>> + Send + Sync>;

/// The event as JSON without its closing `"seq":N}`: `{"event":..,"data":..,"broadcast":..,"seq":`.
fn prefix(event: &str, data: &Value, b: &Broadcast) -> Arc<str> {
    // Go's field order (webSocketEventJSON); serde_json would sort the keys.
    Arc::from(format!(
        r#"{{"event":{},"data":{},"broadcast":{},"seq":"#,
        Value::from(event),
        data,
        serde_json::to_string(b).unwrap()
    ))
}

/// One published event and what `ShouldSendEvent` checks for it, shared by the connections it goes to.
struct Delivery {
    event: String,
    data: Value,
    b: Broadcast,
    full: Arc<str>,
    sanitized: Option<Arc<str>>,
    /// The broadcast names its connection or user: no channel, team or guest check (Go returns before them).
    routed: bool,
    /// `ShouldSendEventToGuest`: the user a guest must be able to see (`user_updated`, `new_user`), when the
    /// event reaches that check (no channel, team, connection or user).
    guest_subject: Option<String>,
}

impl Delivery {
    /// The checks on the connection's session that need no I/O: sensitive data, the team.
    fn session_allows(&self, c: &Conn) -> bool {
        if self.b.admin_only && self.b.required_permissions.is_empty() && !c.admin.load(Ordering::Relaxed) {
            return false;
        }
        if !self.routed
            && !self.b.channel_id.is_empty()
            && matches!(self.event.as_str(), "typing" | "reaction_added" | "reaction_removed")
            && c.presence.read().unwrap().away_from(&self.b.channel_id)
        {
            return false;
        }
        if !self.routed && self.b.channel_id.is_empty() && !self.b.team_id.is_empty() {
            return c.teams.read().unwrap().contains(&self.b.team_id);
        }
        true
    }

    /// Whether a connection whose session is loaded and allowed needs no async check.
    fn is_plain(&self, c: &Conn) -> bool {
        self.b.required_permissions.is_empty()
            && (self.routed || self.b.channel_id.is_empty())
            && self.b.post_hook.is_none()
            && !(self.guest_subject.is_some() && c.guest.load(Ordering::Relaxed))
    }

    /// `ShouldSendEvent` for a connection, with what needs the app: the session reloaded when stale, the required
    /// permissions, channel membership (`GetAllChannelMembersForUser`, cached), the guest's view of the user.
    async fn allows(&self, app: &crate::app::App, c: &Conn) -> bool {
        if !c.authenticate(app).await || !self.session_allows(c) {
            return false;
        }
        if !self.b.required_permissions.is_empty() {
            let s = c.session();
            for p in &self.b.required_permissions {
                if !app.has_perm(&s, p).await {
                    return false;
                }
            }
        }
        if !self.routed && !self.b.channel_id.is_empty() {
            // Archived channels count: Go checks the connection's own copy of the memberships, loaded before the
            // archive (an archive invalidates no user), so the "archived the channel" post still reaches the
            // members. Nothing else can be posted to an archived channel.
            match app.member_roles(&c.user_id, true).await {
                Ok(m) if m.contains_key(&self.b.channel_id) => {}
                _ => return false,
            }
        }
        if let Some(subject) = &self.guest_subject
            && c.guest.load(Ordering::Relaxed)
        {
            // An error means no delivery, as Go.
            return matches!(app.can_see_user(&c.session(), subject).await, Ok(true));
        }
        true
    }

    /// The event for one connection: admin or sanitized data, and the hooks' fields.
    fn render(&self, c: &Conn, post: Option<String>) -> Arc<str> {
        let admin = c.admin.load(Ordering::Relaxed);
        let base = if admin { &self.full } else { self.sanitized.as_ref().unwrap_or(&self.full) };
        let mut extra = self.b.per_user.as_ref().and_then(|f| f(&c.user_id));
        if let Some(p) = post {
            extra.get_or_insert_with(serde_json::Map::new).insert("post".into(), Value::String(p));
        }
        if let Some(a) = &self.b.posted_ack
            && c.posted_ack
            && c.is_active()
            && a.should_ack(&c.user_id, extra.as_ref())
        {
            extra.get_or_insert_with(serde_json::Map::new).insert("should_ack".into(), Value::Bool(true));
        }
        let Some(extra) = extra else { return base.clone() };
        let mut d =
            if admin { self.data.clone() } else { self.b.sanitized.clone().unwrap_or_else(|| self.data.clone()) };
        if let Value::Object(m) = &mut d {
            m.extend(extra);
        }
        prefix(&self.event, &d, &self.b)
    }

    /// The post hook's answer for the user of a connection.
    async fn post_for(&self, app: &State, c: &Conn) -> Option<String> {
        match &self.b.post_hook {
            Some(h) => h(app.clone(), c.user_id.clone()).await,
            None => None,
        }
    }
}

/// `ShouldSendEventToGuest`'s subject: the user of `user_updated` (`data.user.id`), the `user_id` of `new_user`.
/// An event without one goes to no guest (an empty id is a user nobody can see).
fn guest_subject(event: &str, data: &Value) -> Option<String> {
    let id = match event {
        "user_updated" => &data["user"]["id"],
        "new_user" => &data["user_id"],
        _ => return None,
    };
    Some(id.as_str().unwrap_or_default().to_string())
}

impl Hub {
    pub fn set_app(&self, app: &State) {
        let _ = self.app.set(Arc::downgrade(app));
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let weak = Arc::downgrade(app);
        rt.spawn(async move {
            let mut tick = tokio::time::interval(INACTIVE_KEPT);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(app) = weak.upgrade() else { return };
                app.hub.reap();
            }
        });
    }

    /// The server state, for work that outlives a request.
    pub fn app(&self) -> Option<State> {
        self.app.get().and_then(std::sync::Weak::upgrade)
    }

    fn add(&self, c: Arc<Conn>) {
        self.conns.write().unwrap().push(c);
    }
    /// Removes a connection; true when the user has no active one left.
    fn remove(&self, id: u64) -> bool {
        let mut c = self.conns.write().unwrap();
        let Some(i) = c.iter().position(|x| x.id == id) else { return false };
        let gone = c.swap_remove(i);
        !c.iter().any(|x| x.user_id == gone.user_id && x.is_active())
    }

    /// Keeps a connection whose socket is gone for a reconnect; true when the user has no active one left.
    fn park(&self, c: &Conn, p: Parked) -> bool {
        *c.parked.lock().unwrap() = Some(p);
        c.inactive_at.store(now_ms(), Ordering::Relaxed);
        c.active.store(false, Ordering::Relaxed);
        !self.conns.read().unwrap().iter().any(|x| x.user_id == c.user_id && x.is_active())
    }

    /// The parked connection a reconnect names, out of the hub.
    fn take_parked(&self, user_id: &str, connection_id: &str) -> Option<(mpsc::Sender<Out>, Parked)> {
        let mut conns = self.conns.write().unwrap();
        let i = conns
            .iter()
            .position(|c| !c.is_active() && !c.is_dead() && c.user_id == user_id && c.connection_id == connection_id)?;
        let c = conns.swap_remove(i);
        let p = c.parked.lock().unwrap().take()?;
        Some((c.tx.clone(), p))
    }

    fn reap(&self) {
        let oldest = now_ms() - INACTIVE_KEPT.as_millis() as i64;
        self.conns
            .write()
            .unwrap()
            .retain(|c| c.is_active() || (!c.is_dead() && c.inactive_at.load(Ordering::Relaxed) > oldest));
    }

    pub fn is_connected(&self, user_id: &str) -> bool {
        self.conns.read().unwrap().iter().any(|c| c.user_id == user_id && c.is_active())
    }

    pub fn connections(&self) -> usize {
        self.conns.read().unwrap().iter().filter(|c| c.is_active()).count()
    }

    /// Closes the connections of a revoked session; nothing more is sent to them meanwhile.
    pub fn close_session(&self, token: &str) {
        for c in self.conns.read().unwrap().iter().filter(|c| c.token == token) {
            c.close();
        }
    }

    /// `Hub.InvalidateUser` (`WebConn.InvalidateCache`): the user's connections load their session again before
    /// their next event (roles, guest, teams, a revoked or expired session).
    pub fn invalidate_user(&self, user_id: &str) {
        for c in self.conns.read().unwrap().iter().filter(|c| c.user_id == user_id) {
            c.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The connections of one session load it again before their next event (a revoked OAuth token).
    pub fn invalidate_session(&self, token: &str) {
        for c in self.conns.read().unwrap().iter().filter(|c| c.token == token) {
            c.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// `Hub.InvalidateAllCache`: every connection loads its session again before its next event.
    pub fn invalidate_all(&self) {
        for c in self.conns.read().unwrap().iter() {
            c.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The user joined or left a team (`InvalidateCacheForWebConn`).
    pub fn set_user_teams(&self, user_id: &str, teams: &[String]) {
        for c in self.conns.read().unwrap().iter().filter(|c| c.user_id == user_id) {
            *c.teams.write().unwrap() = teams.iter().cloned().collect();
        }
    }

    /// `Publish`: to every connection `ShouldSendEvent` selects. A connection whose session is loaded and that
    /// needs nothing more gets the event here; the others (a stale session, channel membership, required
    /// permissions, a guest's view, a post hook) get it off the caller's path.
    pub fn publish(&self, event: &str, data: Value, b: Broadcast) {
        let now = now_ms();
        let routed = !b.connection_id.is_empty() || !b.user_id.is_empty();
        let subject =
            if !routed && b.channel_id.is_empty() && b.team_id.is_empty() { guest_subject(event, &data) } else { None };
        let mut later: Vec<Arc<Conn>> = Vec::new();
        let mut now_conns: Vec<Arc<Conn>> = Vec::new();
        {
            let conns = self.conns.read().unwrap();
            for c in conns.iter() {
                // The broadcast's own routing, which needs no session.
                if !b.connection_id.is_empty() {
                    if c.connection_id != b.connection_id {
                        continue;
                    }
                } else if !b.omit_connection_id.is_empty() && c.connection_id == b.omit_connection_id {
                    continue;
                } else if !b.user_id.is_empty() {
                    if c.user_id != b.user_id {
                        continue;
                    }
                } else if b.omit_users.as_ref().is_some_and(|o| o.contains_key(&c.user_id)) {
                    continue;
                }
                if c.is_dead() {
                    continue;
                }
                if c.is_stale(now) {
                    later.push(c.clone());
                } else {
                    now_conns.push(c.clone());
                }
            }
        }
        if later.is_empty() && now_conns.is_empty() {
            return;
        }
        let dl = Delivery {
            event: event.to_string(),
            full: prefix(event, &data, &b),
            sanitized: b.sanitized.as_ref().map(|s| prefix(event, s, &b)),
            data,
            b,
            routed,
            guest_subject: subject,
        };
        for c in now_conns {
            if !dl.session_allows(&c) {
                continue;
            }
            if dl.is_plain(&c) {
                c.send(Out::Event(dl.render(&c, None)));
            } else {
                later.push(c);
            }
        }
        if later.is_empty() {
            return;
        }
        let Some(app) = self.app() else { return };
        tokio::spawn(async move {
            for c in later {
                if dl.allows(&app, &c).await {
                    let post = dl.post_for(&app, &c).await;
                    c.send(Out::Event(dl.render(&c, post)));
                }
            }
        });
    }
}

/// `connectWebSocket` behind `APIHandlerTrustRequester`: the session of the request's token (a token Go cannot
/// find is no session: the client may still authenticate on the socket), then gorilla's `Upgrader.Upgrade` with
/// `App.OriginChecker`.
pub async fn connect(St(app): St<State>, req: axum::extract::Request) -> Response {
    use axum::response::IntoResponse;
    let (mut parts, _) = req.into_parts();
    let mut token = None;
    if let Some((t, loc)) = token_from(&parts) {
        match app.session(&t).await {
            // web/handlers.go: a token in the query string is for OAuth apps only.
            Ok(s) if loc == crate::app::TokenLocation::Query && !s.is_oauth => {
                return crate::app::AppError::new(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "api.context.token_provided.app_error",
                    "Session is not OAuth but token was provided in the query string",
                )
                .into_response();
            }
            Ok(_) => token = Some(t),
            Err(e) if e.status_code == 500 => return e.into_response(),
            Err(_) => {}
        }
    }
    let origin = parts.headers.get("origin").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    if let Err(status) = handshake_check(&app, &parts) {
        return upgrade_error(status, origin.as_deref().unwrap_or(""));
    }
    let query = |name: &str| {
        parts
            .uri
            .query()
            .unwrap_or("")
            .split('&')
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=').map(str::to_string))
    };
    let resume = query("connection_id").filter(|c| !c.is_empty()).map(|c| (c, query("sequence_number")));
    let posted_ack = query("posted_ack").as_deref() == Some("true");
    match WebSocketUpgrade::from_request_parts(&mut parts, &app).await {
        Ok(ws) => ws.max_message_size(8 * 1024).on_upgrade(move |socket| serve(app, socket, token, resume, posted_ack)),
        Err(_) => upgrade_error(axum::http::StatusCode::BAD_REQUEST, origin.as_deref().unwrap_or("")),
    }
}

/// gorilla's `tokenListContainsValue`: one of the header's comma-separated tokens, any case.
fn header_has_token(parts: &Parts, name: &str, value: &str) -> bool {
    parts.headers.get_all(name).iter().any(|v| {
        String::from_utf8_lossy(v.as_bytes())
            .split(',')
            .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(value))
    })
}

/// `Upgrader.Upgrade`'s checks, in its order: the status it refuses with.
fn handshake_check(app: &crate::app::App, parts: &Parts) -> Result<(), axum::http::StatusCode> {
    use axum::http::StatusCode;
    if !header_has_token(parts, "connection", "upgrade") || !header_has_token(parts, "upgrade", "websocket") {
        return Err(StatusCode::BAD_REQUEST);
    }
    if parts.method != axum::http::Method::GET {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    if !header_has_token(parts, "sec-websocket-version", "13") {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !origin_allowed(app, parts) {
        return Err(StatusCode::FORBIDDEN);
    }
    let key = parts.headers.get("sec-websocket-key").map(|v| v.as_bytes()).unwrap_or_default();
    use base64::Engine;
    if key.is_empty() || base64::engine::general_purpose::STANDARD.decode(key).map(|k| k.len()) != Ok(16) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(())
}

/// `App.OriginChecker`: with `AllowCorsFrom`, those origins and the site URL (or any, for `*`); without it, the
/// origin must be the site URL's scheme and host (the request's host when there is no site URL). No `Origin` is
/// fine (not a browser), `null` is not.
fn origin_allowed(app: &crate::app::App, parts: &Parts) -> bool {
    let site = app.cfg.str("ServiceSettings.SiteURL");
    let cors = app.cfg.str("ServiceSettings.AllowCorsFrom");
    let first = parts.headers.get("origin").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    if !cors.is_empty() {
        // utils.CheckOrigin
        let origin = first.unwrap_or_default();
        if origin.is_empty() || cors == "*" {
            return true;
        }
        let mut allowed = cors.to_string();
        if let Ok(mut u) = crate::gourl::Url::parse(site) {
            u.raw_path.clear();
            allowed.push(' ');
            allowed.push_str(&u.to_go_string());
        }
        return allowed.split(' ').any(|a| a == origin);
    }
    let Some(origin) = first else { return true };
    if origin == "null" {
        return false;
    }
    let Ok(u) = crate::gourl::Url::parse(&origin) else { return false };
    if site.is_empty() {
        let host = parts.headers.get("host").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let host = host.or_else(|| parts.uri.authority().map(|a| a.to_string())).unwrap_or_default();
        return u.host.eq_ignore_ascii_case(&host);
    }
    let Ok(s) = crate::gourl::Url::parse(site) else { return false };
    u.host.eq_ignore_ascii_case(&s.host) && u.scheme.eq_ignore_ascii_case(&s.scheme)
}

/// A refused upgrade as Go answers it: gorilla's `http.Error` (its status, `Sec-Websocket-Version: 13`, the
/// status text), then the handler's `api.web_socket.connect.upgrade.app_error` (400 in the JSON) written after it.
fn upgrade_error(status: axum::http::StatusCode, origin: &str) -> Response {
    use axum::http::{HeaderValue, header};
    let e =
        crate::app::AppError::new(axum::http::StatusCode::BAD_REQUEST, "api.web_socket.connect.upgrade.app_error", "")
            .with_param("BlockedOrigin", origin);
    let rid = e.request_id.clone();
    let json = serde_json::to_string(&e).unwrap_or_default();
    let body = format!("{}\n{json}", status.canonical_reason().unwrap_or(""));
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert("sec-websocket-version", HeaderValue::from_static("13"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&rid) {
        h.insert("x-request-id", v);
    }
    r
}

/// A client's request: the frame's first JSON value, its fields matched in any case; None when it is not one.
struct WsRequest {
    seq: i64,
    action: String,
    data: Value,
}

impl WsRequest {
    fn parse(text: &str) -> Option<WsRequest> {
        let v = serde_json::Deserializer::from_str(text).into_iter::<Value>().next()?.ok()?;
        let o = v.as_object()?;
        let field = |k: &str| {
            o.get(k)
                .or_else(|| o.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v))
                .filter(|v| !v.is_null())
        };
        Some(WsRequest {
            seq: match field("seq") {
                Some(v) => v.as_i64()?,
                None => 0,
            },
            action: match field("action") {
                Some(v) => v.as_str()?.to_string(),
                None => String::new(),
            },
            data: match field("data") {
                Some(v) if v.is_object() => v.clone(),
                Some(_) => return None,
                None => Value::Null,
            },
        })
    }
}

/// The token and seq of `authentication_challenge`; other requests get no answer meanwhile.
async fn challenge(stream: &mut futures_util::stream::SplitStream<WebSocket>) -> Option<(String, i64)> {
    let deadline = tokio::time::Instant::now() + AUTH_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await.ok()??.ok()? {
            Message::Text(t) => {
                let req = WsRequest::parse(&t)?;
                if req.seq > 0 && req.action == "authentication_challenge" {
                    return Some((req.data["token"].as_str()?.to_string(), req.seq));
                }
            }
            Message::Binary(_) | Message::Close(_) => return None,
            _ => {}
        }
    }
}

async fn serve(
    app: State,
    socket: WebSocket,
    token: Option<String>,
    resume: Option<(String, Option<String>)>,
    posted_ack: bool,
) {
    let (mut sink, mut stream) = socket.split();
    let (token, challenge_seq) = match token {
        Some(t) => (Some(t), 0),
        None => challenge(&mut stream).await.map_or((None, 0), |(t, seq)| (Some(t), seq)),
    };
    let Some(session) = (match &token {
        Some(t) => app.session(t).await.ok(),
        None => None,
    }) else {
        return;
    };
    let user_id = session.user_id.clone();
    let mut connection_id = new_id();
    let mut reused = None;
    if challenge_seq == 0
        && let Some((id, seq)) = resume
    {
        let Some(seq) = seq.and_then(|s| s.parse::<i64>().ok()).filter(|_| crate::app::is_valid_id(&id)) else {
            return;
        };
        if let Some(found) = app.hub.take_parked(&user_id, &id) {
            connection_id = id;
            reused = Some((found, seq));
        }
    }
    let fresh = reused.is_none();
    let (tx, mut rx, mut sent, want) = match reused {
        Some(((tx, p), seq)) => (tx, p.rx, p.sent, seq),
        None => {
            let (tx, rx) = mpsc::channel::<Out>(SEND_QUEUE);
            (tx, rx, Sent::default(), 0)
        }
    };
    let mut seq = want;
    let mut replay = None;
    let mut new_hello = None;
    if want != 0 {
        if let Some(i) = sent.find(want) {
            replay = Some(i);
        } else if sent.lost(want) {
            sent = Sent::default();
            connection_id = new_id();
            seq = 0;
            new_hello = Some(hello(&user_id, &connection_id));
        }
    }
    let mut conn = Conn::new(app.hub.next.fetch_add(1, Ordering::Relaxed), session.clone(), tx, connection_id);
    conn.posted_ack = posted_ack;
    let conn = Arc::new(conn);
    app.hub.add(conn.clone());

    let closing = conn.closing.clone();
    let reader_done = Arc::new(tokio::sync::Notify::new());
    let quiet = reader_done.clone();
    let mut writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let mut ok = true;
        let mut close_frame = true;
        if let Some(i) = replay {
            for (s, e) in sent.0.iter().skip(i) {
                if sink.send(Message::Text(frame(e, *s))).await.is_err() {
                    ok = false;
                    break;
                }
                seq += 1;
            }
        }
        if let Some(h) = new_hello {
            sent.push(seq, h.clone());
            ok = ok && sink.send(Message::Text(frame(&h, seq))).await.is_ok();
            seq += 1;
        }
        while ok {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(Out::Event(e)) => {
                        sent.push(seq, e.clone());
                        ok = sink.send(Message::Text(frame(&e, seq))).await.is_ok();
                        seq += 1;
                    }
                    Some(Out::Reply(r)) => ok = sink.send(Message::Text(r.into())).await.is_ok(),
                    None => ok = false,
                },
                _ = ping.tick() => ok = sink.send(Message::Ping(Default::default())).await.is_ok(),
                _ = closing.notified() => ok = false,
                _ = quiet.notified() => {
                    ok = false;
                    close_frame = false;
                }
            }
        }
        if close_frame {
            let _ = sink.close().await;
        }
        Parked { rx, sent }
    });

    if fresh {
        conn.send(Out::Event(hello(&user_id, &conn.connection_id)));
    }
    if challenge_seq > 0 {
        reply_ok(&conn, challenge_seq, None);
    }
    // `NewWebConn`: `UpdateLastActivityAtIfNeeded` once, with the session just loaded.
    app.touch_session(&session).await;
    app.set_status_online(&user_id, false).await;

    // `WebConn.readPump`: the read deadline is `pongWaitTime` from the connect and moves only with a pong, so a
    // client that stops answering the pings (gone without a close, or never reading) is dropped and goes offline.
    let mut deadline = tokio::time::Instant::now() + PONG_WAIT;
    let mut done = None;
    let mut client_closed = false;
    loop {
        // The writer ending (closed, full queue, broken socket) ends the connection without waiting for the
        // client's next frame.
        let m = tokio::select! {
            m = stream.next() => m,
            r = &mut writer => {
                done = Some(r);
                break;
            }
            _ = tokio::time::sleep_until(deadline) => break,
        };
        let Some(Ok(m)) = m else { break };
        match m {
            // The pong handler: a new deadline, and the user goes away once idle past `UserStatusAwayTimeout`.
            Message::Pong(_) => {
                deadline = tokio::time::Instant::now() + PONG_WAIT;
                let (app, user_id) = (app.clone(), user_id.clone());
                tokio::spawn(async move { app.set_status_away_if_needed(&user_id, false).await });
            }
            Message::Text(t) => {
                let Some(req) = WsRequest::parse(&t) else { break };
                if req.action.is_empty() {
                    reply_error(&conn, req.seq, WsError::NoAction);
                } else if req.seq <= 0 {
                    reply_error(&conn, req.seq, WsError::BadSeq);
                } else if req.action == "presence" {
                    conn.set_presence(&req.data);
                    reply_ok(&conn, req.seq, None);
                } else if req.action != "authentication_challenge" {
                    // `ServeWebSocket`: every request gets the session as it is now (cache or database), never the
                    // one of the connect: a revoked session fails, a changed one carries its new roles.
                    match app.session(&conn.token).await {
                        Ok(fresh) => action(&app, &fresh, &conn, &req).await,
                        Err(e) => session_error_reply(&conn, req.seq, e),
                    }
                }
            }
            Message::Close(_) => {
                client_closed = true;
                break;
            }
            Message::Binary(_) => break,
            _ => {}
        }
    }
    let parked = match done {
        Some(r) => r.ok(),
        None => {
            if client_closed {
                conn.closing.notify_one();
            } else {
                reader_done.notify_one();
            }
            match tokio::time::timeout(WRITE_WAIT, &mut writer).await {
                Ok(r) => r.ok(),
                Err(_) => {
                    writer.abort();
                    None
                }
            }
        }
    };
    let last = match parked {
        Some(p) if !conn.is_dead() => app.hub.park(&conn, p),
        _ => app.hub.remove(conn.id),
    };
    if last {
        app.queue_status_offline(&user_id, false).await;
    }
}

/// `model.WebSocketResponse`, in Go's field order.
#[derive(Serialize)]
struct WsReply {
    status: &'static str,
    #[serde(skip_serializing_if = "is_zero")]
    seq_reply: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WsReplyError>,
}

/// The `AppError` of a failed socket request, its detail wiped (`WipeDetailed`).
#[derive(Serialize)]
struct WsReplyError {
    id: String,
    message: String,
    detailed_error: &'static str,
    status_code: u16,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Why a socket request failed: `NewInvalidWebSocketParamError`, `NewServerBusyWebSocketError`.
enum WsError {
    Param(&'static str),
    Busy,
    BadAction,
    NoAction,
    BadSeq,
}

fn reply_error(conn: &Conn, seq_reply: i64, e: WsError) {
    let (id, status_code, args): (&str, u16, &[(&str, &str)]) = match &e {
        WsError::Param(name) => ("api.websocket_handler.invalid_param.app_error", 400, &[("Name", name)]),
        WsError::Busy => ("api.websocket_handler.server_busy.app_error", 503, &[]),
        WsError::BadAction => ("api.web_socket_router.bad_action.app_error", 500, &[]),
        WsError::NoAction => ("api.web_socket_router.no_action.app_error", 400, &[]),
        WsError::BadSeq => ("api.web_socket_router.bad_seq.app_error", 400, &[]),
    };
    let message = crate::i18n::tr(id, args).unwrap_or_else(|| id.to_string());
    let error = WsReplyError { id: id.to_string(), message, detailed_error: "", status_code };
    let msg = WsReply { status: "FAIL", seq_reply, data: None, error: Some(error) };
    conn.send(Out::Reply(serde_json::to_string(&msg).unwrap_or_default()));
}

/// `ServeWebSocket` when the connection's session is gone or invalid: the error reply, its detail wiped.
fn session_error_reply(conn: &Conn, seq_reply: i64, e: crate::app::AppError) {
    let error = WsReplyError { id: e.id, message: e.message, detailed_error: "", status_code: e.status_code };
    let msg = WsReply { status: "FAIL", seq_reply, data: None, error: Some(error) };
    conn.send(Out::Reply(serde_json::to_string(&msg).unwrap_or_default()));
}

/// A client's request on the socket (`wsapi`), answered with `{"status","seq_reply"}`.
async fn action(app: &State, s: &Arc<Session>, conn: &Conn, req: &WsRequest) {
    let data = &req.data;
    let reply = match req.action.as_str() {
        "user_typing" => 'typing: {
            app.extend_session_if_needed(s).await;
            if crate::admin::is_busy() {
                // A non-critical service, off while the server is busy.
                break 'typing Err(WsError::Busy);
            }
            let channel_id = data["channel_id"].as_str().unwrap_or("");
            if crate::app::is_valid_id(channel_id)
                && app.channel_perm(s, channel_id, crate::perm::CREATE_POST).await.is_ok_and(|p| p.0)
            {
                publish_typing(app, &s.user_id, channel_id, data["parent_id"].as_str().unwrap_or(""));
                Ok(None)
            } else {
                Err(WsError::Param("channel_id"))
            }
        }
        "user_update_active_status" => match data["user_is_active"].as_bool() {
            Some(active) => {
                let manual = data["manual"].as_bool().unwrap_or(false);
                if active {
                    app.set_status_online(&s.user_id, manual).await;
                } else {
                    app.set_status_away_if_needed(&s.user_id, manual).await;
                }
                Ok(None)
            }
            None => Err(WsError::Param("user_is_active")),
        },
        // `GetAllStatuses`: every status in the cache, id -> status.
        "get_statuses" => Ok(Some(Value::Object(app.all_cached_statuses().into_iter().collect()))),
        "get_statuses_by_ids" => {
            let ids: Vec<String> = data["user_ids"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                Err(WsError::Param("user_ids"))
            } else {
                let st = app.statuses(&ids).await.unwrap_or_default();
                Ok(Some(Value::Object(st.into_iter().map(|s| (s.user_id, Value::String(s.status))).collect())))
            }
        }
        "ping" => Ok(Some(
            json!({"text": "pong", "version": crate::SERVER_VERSION_FULL, "server_time": now_ms(), "node_id": ""}),
        )),
        "posted_notify_ack" => Ok(None),
        // `WebSocketRouter.ServeWebSocket`: no handler for the action.
        _ => Err(WsError::BadAction),
    };
    match reply {
        Ok(data) => reply_ok(conn, req.seq, data),
        Err(e) => reply_error(conn, req.seq, e),
    }
}

fn reply_ok(conn: &Conn, seq_reply: i64, data: Option<Value>) {
    let msg = WsReply { status: "OK", seq_reply, data, error: None };
    conn.send(Out::Reply(serde_json::to_string(&msg).unwrap_or_default()));
}

/// `PublishUserTyping`.
pub fn publish_typing(app: &State, user_id: &str, channel_id: &str, parent_id: &str) {
    let mut omit = BTreeMap::new();
    omit.insert(user_id.to_string(), true);
    app.hub.publish(
        "typing",
        json!({"parent_id": parent_id, "user_id": user_id}),
        Broadcast { channel_id: channel_id.into(), omit_users: Some(omit), ..Default::default() },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(s: Session) -> (Conn, mpsc::Receiver<Out>) {
        let (tx, rx) = mpsc::channel(SEND_QUEUE);
        (Conn::new(1, Arc::new(s), tx, new_id()), rx)
    }

    #[test]
    fn stale_after_invalidation_or_expiry() {
        let now = now_ms();
        let (c, _rx) =
            conn(Session { user_id: "u".into(), token: "t".into(), expires_at: now + 60_000, ..Default::default() });
        assert!(!c.is_stale(now));
        // InvalidateCache
        c.generation.fetch_add(1, Ordering::AcqRel);
        assert!(c.is_stale(now));
        c.loaded.store(1, Ordering::Release);
        assert!(!c.is_stale(now));
        // Past its expiry; 0 never expires.
        assert!(c.is_stale(now + 120_000));
        c.expires_at.store(0, Ordering::Relaxed);
        assert!(!c.is_stale(now + 120_000));
    }

    #[test]
    fn session_flags() {
        let mut s = Session { user_id: "u".into(), roles: "system_user system_admin".into(), ..Default::default() };
        s.props.0.insert("is_guest".into(), "true".into());
        let (c, _rx) = conn(s);
        assert!(c.admin.load(Ordering::Relaxed) && c.guest.load(Ordering::Relaxed));
        c.set_session(Arc::new(Session { user_id: "u".into(), roles: "system_user".into(), ..Default::default() }));
        assert!(!c.admin.load(Ordering::Relaxed) && !c.guest.load(Ordering::Relaxed));
    }

    #[test]
    fn guest_subjects() {
        assert_eq!(guest_subject("user_updated", &json!({"user": {"id": "a"}})).as_deref(), Some("a"));
        assert_eq!(guest_subject("new_user", &json!({"user_id": "b"})).as_deref(), Some("b"));
        assert_eq!(guest_subject("user_updated", &json!({})).as_deref(), Some(""));
        assert_eq!(guest_subject("posted", &json!({"user_id": "b"})), None);
    }

    #[test]
    fn sent_keeps_the_last_events() {
        let mut s = Sent::default();
        for i in 0..200 {
            s.push(i, Arc::from("e"));
        }
        assert_eq!(s.0.len(), SENT_KEPT);
        assert_eq!(s.find(72), Some(0));
        assert_eq!(s.find(71), None);
        assert!(!s.lost(200));
        assert!(s.lost(150) && s.lost(300));
        assert!(!Sent::default().lost(5));
    }

    #[test]
    fn requests() {
        let r = WsRequest::parse(r#"{"SEQ":2,"Action":"ping","data":{"a":1}} trailing"#).unwrap();
        assert_eq!((r.seq, r.action.as_str(), r.data["a"].as_i64()), (2, "ping", Some(1)));
        assert_eq!(WsRequest::parse(r#"{"seq":5}"#).map(|r| r.action), Some(String::new()));
        for bad in ["hello", "[1,2]", r#"{"seq":"1","action":"ping"}"#, r#"{"seq":1.5}"#, r#"{"data":5}"#] {
            assert!(WsRequest::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parked_connection_is_taken_once() {
        let hub = Hub::default();
        let (c, _rx) = conn(Session { user_id: "u".into(), token: "t".into(), ..Default::default() });
        let c = Arc::new(c);
        hub.add(c.clone());
        let (_tx, rx) = mpsc::channel(1);
        assert!(hub.park(&c, Parked { rx, sent: Sent::default() }));
        assert!(!hub.is_connected("u"));
        assert!(hub.take_parked("other", &c.connection_id).is_none());
        assert!(hub.take_parked("u", &c.connection_id).is_some());
        assert!(hub.take_parked("u", &c.connection_id).is_none());
    }

    #[test]
    fn presence_filters_only_when_every_view_is_elsewhere() {
        let mut p = Presence::default();
        assert!(!p.away_from("c"));
        p.channel = Some("d".into());
        assert!(!p.away_from("c"));
        p.rhs_thread_channel = Some("d".into());
        p.thread_view_channel = Some(String::new());
        assert!(p.away_from("c"));
        p.thread_view_channel = Some("c".into());
        assert!(!p.away_from("c"));
    }

    #[test]
    fn should_ack_mentions_followers_direct_and_notified() {
        let a = PostedAck { poster: "p".into(), direct: false, users: HashSet::from(["u".to_string()]) };
        assert!(!a.should_ack("p", None));
        assert!(a.should_ack("u", None));
        assert!(!a.should_ack("x", None));
        let mut extra = serde_json::Map::new();
        extra.insert("followers".into(), Value::String("[\"x\"]".into()));
        assert!(a.should_ack("x", Some(&extra)));
        let d = PostedAck { direct: true, ..a };
        assert!(d.should_ack("x", None));
        assert!(!d.should_ack("p", None));
    }
}
