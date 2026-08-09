//! Background session driver and handle.
//!
//! [`spawn_session`] takes a [`SessionConfig`] (account + refresh token + proxy),
//! establishes a logged-on CM connection, and moves it onto its own task. The
//! task owns the socket: it heartbeats, reads incoming messages, routes replies
//! to in-flight requests by job id, broadcasts the rest, and — on a transport
//! failure — **reconnects automatically** with exponential backoff. Callers
//! interact through the cloneable [`SessionHandle`].
//!
//! This is the concurrency model for fleets: many self-healing sessions, each a
//! cheap task, with `request`/`notify`/`subscribe` usable from any handle clone.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use prost::Message as _;
use tracing::{debug, error, info, trace, warn, Instrument};

use crate::auth::RefreshToken;
use crate::codec::{self, SteamMessage};
use crate::error::eresult;
use crate::proto::{CMsgClientLoggedOff, CMsgProtoBufHeader};
use crate::session::connection::{
    read_message, write_frame, write_ping, CmConnection, Liveness, LoggedOn, EMSG_CLIENT_HEARTBEAT,
    EMSG_CLIENT_LOGGED_OFF,
};
use crate::session::state::SessionState;
use crate::transport::proxy::ProxyConfig;
use crate::transport::websocket::SteamWebSocket;
use crate::{Error, Result};

/// Outstanding-request and notification queue depth.
const COMMAND_CAPACITY: usize = 64;
/// Buffered unsolicited messages per subscriber before lag.
const EVENT_CAPACITY: usize = 256;
/// Backoff before the first reconnect attempt.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound on reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Attempts for the *initial* connect before [`spawn_session`] gives up. Each
/// attempt rediscovers and reconnects, so a flaky rotating proxy gets several
/// fresh exits to land a good one rather than failing the caller on one bad exit.
const INITIAL_CONNECT_ATTEMPTS: u32 = 4;
// k_EMsgClientLogOff, from protos/steam/enums_clientserver.proto.
const EMSG_CLIENT_LOGOFF: u32 = 706;
/// Default budget for a [`SessionHandle::request`] before it fails with
/// [`Error::Timeout`]. Steam answers well inside this; a job that doesn't come
/// back at all would otherwise hang the caller forever.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the driver reaps timed-out and abandoned pending requests.
const PENDING_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// How many unanswered Ping probes make the socket dead rather than quiet.
///
/// Every heartbeat tick the driver sends a WebSocket Ping alongside the Steam
/// `CMsgClientHeartBeat`, and the CM answers it with a Pong that stamps the
/// connection's [`Liveness`]. So the probe interval *is* the heartbeat
/// interval, and this many of them passing with **no inbound frame of any
/// kind** (control or application) is positive evidence of a dead-but-open
/// socket (the classic rotating-proxy failure), not merely of an idle account.
const IDLE_PROBE_MISSES: u32 = 6;
/// Floor under the read-idle budget, in case Steam hands us a very short
/// heartbeat interval.
const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for the goodbye `Close` write during teardown. A half-open proxy exit
/// can leave the flush pending forever, and nothing is waiting on it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

// One-shot snapshots Steam pushes exactly once after each logon: the full
// friends list (`k_EMsgClientFriendsList` = 767), friends-groups list (5553)
// and player-nickname list (5587). The full snapshot always precedes any
// incremental delta on the same emsg, so the driver caches the **first** body
// it sees per emsg since the last logon (see [`SnapshotCache`]) — a later delta
// never overwrites it. This lets [`crate::friends`]'s `request_*` read the
// snapshot race-free instead of having to subscribe the instant the session
// comes up, only to find the push already broadcast and gone.
const POST_LOGIN_SNAPSHOT_EMSGS: [u32; 3] = [767, 5553, 5587];

/// First-wins-per-emsg cache of the [`POST_LOGIN_SNAPSHOT_EMSGS`] bodies for the
/// current logon. Shared between the driver (the only writer) and every
/// [`SessionHandle`] (readers); cleared on every (re)logon because Steam
/// re-pushes the snapshots. Critical sections are a single map op — never held
/// across an `.await` — so a blocking `std::sync::Mutex` is the right fit.
type SnapshotCache = Arc<Mutex<HashMap<u32, Vec<u8>>>>;

/// Everything the driver needs to establish — and re-establish — a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Steam account name.
    pub account_name: String,
    /// Refresh token from the [`auth`](crate::auth) flow (a `SteamClient` token).
    pub refresh_token: RefreshToken,
    /// Optional proxy for discovery and the CM connection.
    pub proxy: Option<ProxyConfig>,
}

// public only under the test seam
#[cfg(any(test, feature = "test-seam"))]
pub use self::command::Command;
#[cfg(not(any(test, feature = "test-seam")))]
pub(crate) use self::command::Command;

mod command {
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    use crate::codec::SteamMessage;
    use crate::Result;

    /// A request/notification queued from a [`super::SessionHandle`] to the
    /// driver.
    ///
    /// Reachable outside the crate only under the `test-seam` feature, so
    /// [`super::SessionHandle::for_test`] can hand a test the receiving end.
    #[doc(hidden)]
    pub enum Command {
        /// Send and correlate a reply by job id, failing at `deadline`.
        Request {
            emsg: u32,
            body: Vec<u8>,
            deadline: Instant,
            reply: oneshot::Sender<Result<SteamMessage>>,
        },
        /// Send with no reply expected; `ack` carries the write result.
        Notify {
            emsg: u32,
            body: Vec<u8>,
            ack: oneshot::Sender<Result<()>>,
        },
        /// Cleanly log off: tell Steam, then stop the driver.
        Logoff { ack: oneshot::Sender<()> },
    }
}

/// An in-flight request the driver is holding open for a reply.
struct Pending {
    reply: oneshot::Sender<Result<SteamMessage>>,
    deadline: Instant,
}

/// Why the connected loop ended.
enum LoopExit {
    /// All handles dropped — stop for good.
    Shutdown,
    /// Steam logged us off for a terminal reason — stop for good. Carries a
    /// human-readable reason (the `EResult`).
    LoggedOff(String),
    /// Transport failure, or a *transient* server-side logoff — reconnect
    /// (which, through a rotating proxy, lands on a fresh exit / CM).
    Disconnected,
}

/// A cloneable handle to a running, self-healing CM session.
///
/// Cloning is cheap; every clone talks to the same background task. The session
/// stays alive (reconnecting as needed) until **all** handles are dropped, Steam
/// logs it off, or the refresh token is rejected.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<SteamMessage>,
    state: watch::Receiver<SessionState>,
    snapshots: SnapshotCache,
    steam_id: u64,
}

impl SessionHandle {
    /// The logged-on account's 64-bit `SteamID` (stable across reconnects).
    pub fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// A snapshot of the current session state — `LoggedOn` while connected,
    /// `Connecting` mid-reconnect, `LoggedOff` / `Failed` once it has stopped.
    pub fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }

    /// A [`watch::Receiver`] to await state transitions (e.g. to react when the
    /// session drops and starts reconnecting). Useful for fleet monitoring.
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }

    /// Send `req` and await the reply Steam correlates by job id, decoding its
    /// body as `Resp`. Gives up after 30s; use [`Self::request_with_timeout`]
    /// for a different budget.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver stopped or the session dropped the
    /// request mid-reconnect, [`Error::Timeout`] if Steam never answered,
    /// [`Error::Remote`] if the reply header carries a non-OK `EResult`, plus
    /// any transport / decode error.
    pub async fn request<Req, Resp>(&self, emsg: u32, req: &Req) -> Result<Resp>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        self.request_with_timeout(emsg, req, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// [`Self::request`] with an explicit budget. Enforced on both sides: the
    /// caller's future gives up at `timeout`, and the driver holds the same
    /// deadline (reaped on a 1s tick) so an abandoned or expired request also
    /// releases its slot.
    ///
    /// # Errors
    ///
    /// As [`Self::request`].
    pub async fn request_with_timeout<Req, Resp>(
        &self,
        emsg: u32,
        req: &Req,
        timeout: Duration,
    ) -> Result<Resp>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        self.dispatch_request(emsg, req, timeout, true).await
    }

    /// [`Self::request`] **without** the reply-header `EResult` check, for
    /// callers that read the result out of the response body themselves.
    ///
    /// [`Self::request`] fails any reply whose routing header carries a non-OK
    /// `EResult`, which is right for the common case but wrong wherever a
    /// non-OK code is a documented success. `AddFriend` is the example: it
    /// answers `DuplicateRequest` (29) for "already friends", which
    /// [`crate::friends::add_friend`] reports as a successful add. A caller
    /// like that must decode the body and judge for itself, so it takes this
    /// variant. Only such callers should.
    ///
    /// # Errors
    ///
    /// As [`Self::request`], minus [`Error::Remote`].
    pub async fn request_ignoring_eresult<Req, Resp>(&self, emsg: u32, req: &Req) -> Result<Resp>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        self.dispatch_request(emsg, req, DEFAULT_REQUEST_TIMEOUT, false)
            .await
    }

    /// Queue a request, await its reply under `timeout`, optionally reject a
    /// non-OK header `EResult`, and decode the body.
    async fn dispatch_request<Req, Resp>(
        &self,
        emsg: u32,
        req: &Req,
        timeout: Duration,
        check_eresult: bool,
    ) -> Result<Resp>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Request {
                emsg,
                body: req.encode_to_vec(),
                deadline: Instant::now() + timeout,
                reply,
            })
            .await
            .map_err(|_| Error::WebSocket("session driver stopped".into()))?;
        // Caller-side timer for sub-sweep precision; dropping `rx` on expiry
        // closes the slot, which the driver's sweep then reaps.
        let msg = match tokio::time::timeout(timeout, rx).await {
            Ok(received) => received
                .map_err(|_| Error::WebSocket("session driver dropped the request".into()))??,
            Err(_) => return Err(Error::Timeout("steam request")),
        };
        if check_eresult {
            check_reply_eresult(&msg)?;
        }
        Resp::decode(msg.body.as_slice()).map_err(|e| Error::Codec(format!("decode response: {e}")))
    }

    /// Send `req` without expecting a reply, awaiting the actual socket write.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver has stopped or the write failed.
    pub async fn notify<Req: prost::Message>(&self, emsg: u32, req: &Req) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        self.commands
            .send(Command::Notify {
                emsg,
                body: req.encode_to_vec(),
                ack,
            })
            .await
            .map_err(|_| Error::WebSocket("session driver stopped".into()))?;
        rx.await
            .map_err(|_| Error::WebSocket("session driver dropped the notification".into()))?
    }

    /// Subscribe to unsolicited messages (notifications Steam pushes that aren't
    /// replies to a [`Self::request`]). Late subscribers miss earlier messages.
    pub fn subscribe(&self) -> broadcast::Receiver<SteamMessage> {
        self.events.subscribe()
    }

    /// The cached body of a one-shot post-login snapshot Steam pushes once after
    /// logon (the full friends list, friends-groups list or player-nickname
    /// list), or `None` if that snapshot hasn't arrived yet on this logon.
    ///
    /// Lets a caller read a snapshot it may have missed on [`Self::subscribe`]
    /// without racing the post-login push. To stay race-free, **subscribe
    /// first, then read the cache**: any snapshot not yet cached is still
    /// guaranteed to reach the subscription. The cache is cleared and repopulated
    /// on every reconnect. Only the post-login snapshot emsgs are cached.
    pub fn cached_snapshot(&self, emsg: u32) -> Option<Vec<u8>> {
        self.snapshots
            .lock()
            .expect("snapshot cache mutex poisoned")
            .get(&emsg)
            .cloned()
    }

    /// Cleanly log the session off: send `CMsgClientLogOff` to Steam and tear
    /// the socket down. The session stops afterwards (the driver's `JoinHandle`
    /// resolves to `Ok(())`).
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver has already stopped.
    pub async fn logoff(&self) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        self.commands
            .send(Command::Logoff { ack })
            .await
            .map_err(|_| Error::WebSocket("session driver stopped".into()))?;
        // Ignore the result: if the driver ended before acking, we're done anyway.
        let _ = rx.await;
        Ok(())
    }

    /// **Test-only.** Build a handle with no driver behind it, returning the
    /// pieces a fake driver needs: the command receiver, the event sender and
    /// the post-login snapshot cache.
    ///
    /// This exists so `friends` / `persona` / `cs2` / `gc` request builders can
    /// be covered offline: a test drives the returned receiver and answers
    /// commands itself. Not part of the supported API: it may change or vanish
    /// in any release, and it is useless in production code. Compiled only
    /// under `cfg(test)` or the `test-seam` feature.
    #[cfg(any(test, feature = "test-seam"))]
    #[doc(hidden)]
    pub fn for_test(
        steam_id: u64,
    ) -> (
        Self,
        mpsc::Receiver<Command>,
        broadcast::Sender<SteamMessage>,
        SnapshotCache,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (evt_tx, _evt_rx) = broadcast::channel(EVENT_CAPACITY);
        let (_state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id });
        let snapshots: SnapshotCache = Arc::new(Mutex::new(HashMap::new()));
        (
            Self {
                commands: cmd_tx,
                events: evt_tx.clone(),
                state: state_rx,
                snapshots: snapshots.clone(),
                steam_id,
            },
            cmd_rx,
            evt_tx,
            snapshots,
        )
    }
}

/// Fail a reply whose header carries a non-OK `EResult`, folding in Steam's
/// `error_message` when it sent one. Without this a Steam-side rejection decodes
/// as an empty/partial body and reads as success.
fn check_reply_eresult(msg: &SteamMessage) -> Result<()> {
    match msg.header.eresult {
        Some(er) if er != eresult::OK => {
            let emsg = msg.emsg;
            Err(Error::Remote(match msg.header.error_message.as_deref() {
                Some(text) if !text.is_empty() => {
                    format!("emsg {emsg}: eresult {er}: {text}")
                }
                _ => format!("emsg {emsg}: eresult {er}"),
            }))
        }
        _ => Ok(()),
    }
}

/// Establish a session from `config` and move it onto a background task.
///
/// The initial connect + logon happens here, so credential / connectivity
/// errors surface immediately. After that the driver keeps the session alive,
/// reconnecting with backoff on transport drops. The returned [`JoinHandle`]
/// resolves to `Ok(())` when all handles drop or Steam logs off, or `Err` if the
/// refresh token is rejected on reconnect.
///
/// # Errors
///
/// [`Error::AuthRejected`] if the token is rejected, otherwise the last connect
/// error after several attempts (each rediscovering through a fresh proxy exit).
pub async fn spawn_session(
    config: SessionConfig,
) -> Result<(SessionHandle, JoinHandle<Result<()>>)> {
    let (conn, logged) = establish_resilient(&config).await?;

    let steam_id = logged.steam_id;
    let (ws, _steam_id, session_id, inbox, liveness) = conn.into_parts();
    let (write, read) = ws.split();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (evt_tx, _evt_rx) = broadcast::channel(EVENT_CAPACITY);
    let (state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id });
    let snapshots: SnapshotCache = Arc::new(Mutex::new(HashMap::new()));

    info!(steam_id, account = %config.account_name, "session established");
    let driver = SessionDriver {
        write,
        read,
        inbox,
        liveness,
        steam_id,
        session_id,
        heartbeat: logged.heartbeat_interval,
        next_jobid: 1,
        pending: HashMap::new(),
        commands: cmd_rx,
        events: evt_tx.clone(),
        state: state_tx,
        snapshots: snapshots.clone(),
        config,
    };
    // One span per session so every driver event is correlated by account / id.
    let span = tracing::info_span!("session", account = %driver.config.account_name, steam_id);
    let join = tokio::spawn(driver.run().instrument(span));

    Ok((
        SessionHandle {
            commands: cmd_tx,
            events: evt_tx,
            state: state_rx,
            snapshots,
            steam_id,
        },
        join,
    ))
}

/// Establish the initial connection, retrying transient failures with backoff.
///
/// Each attempt rediscovers CMs and opens fresh connections, so on a rotating
/// proxy every retry routes through a new exit — the heal for "this exit is bad,
/// try the next." A rejected token stops immediately (retrying won't help).
async fn establish_resilient(config: &SessionConfig) -> Result<(CmConnection, LoggedOn)> {
    let mut backoff = INITIAL_BACKOFF;
    let mut last_err = Error::Network("no connect attempt ran".into());
    for attempt in 1..=INITIAL_CONNECT_ATTEMPTS {
        match CmConnection::establish(
            &config.account_name,
            config.refresh_token.expose(),
            config.proxy.as_ref(),
        )
        .await
        {
            Ok(ok) => return Ok(ok),
            Err(e @ Error::AuthRejected(_)) => return Err(e),
            Err(e) => {
                last_err = e;
                if attempt < INITIAL_CONNECT_ATTEMPTS {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
    Err(last_err)
}

/// Owns the socket on the background task.
struct SessionDriver {
    write: SplitSink<SteamWebSocket, WsMessage>,
    read: SplitStream<SteamWebSocket>,
    inbox: VecDeque<SteamMessage>,
    /// When the socket last had any frame arrive on it (see the read-idle
    /// watchdog in [`SessionDriver::run_connected`]).
    liveness: Liveness,
    steam_id: u64,
    session_id: i32,
    heartbeat: Duration,
    next_jobid: u64,
    pending: HashMap<u64, Pending>,
    commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<SteamMessage>,
    state: watch::Sender<SessionState>,
    snapshots: SnapshotCache,
    config: SessionConfig,
}

impl SessionDriver {
    async fn run(mut self) -> Result<()> {
        let result = loop {
            match self.run_connected().await {
                LoopExit::Shutdown => {
                    break Ok("session closed".to_string());
                }
                LoopExit::LoggedOff(reason) => {
                    break Ok(reason);
                }
                LoopExit::Disconnected => {
                    // Messages already unpacked from a Multi are real and still
                    // deliverable; dispatch them before the socket goes away.
                    while let Some(msg) = self.inbox.pop_front() {
                        dispatch(&mut self.pending, &self.events, &self.snapshots, msg);
                    }
                    // In-flight requests can't be answered on a dead socket.
                    warn!(
                        pending = self.pending.len(),
                        "session disconnected, reconnecting"
                    );
                    fail_all_pending(&mut self.pending, "session reconnecting");
                    let _ = self.state.send(SessionState::Connecting);
                    match self.reconnect().await {
                        Ok(Reconnect::Stopped) => break Ok("session closed".to_string()),
                        Ok(Reconnect::Live) => {
                            info!(steam_id = self.steam_id, "session reconnected");
                            // Steam re-pushes the post-login snapshots on the new
                            // logon; drop the stale ones so the cache repopulates.
                            self.snapshots
                                .lock()
                                .expect("snapshot cache mutex poisoned")
                                .clear();
                            let _ = self.state.send(SessionState::LoggedOn {
                                steam_id: self.steam_id,
                            });
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
        };

        // Every exit path frees its callers. The Disconnected arm already failed
        // them before reconnecting; a logoff, a shutdown or a fatal reconnect
        // error would otherwise hold them until `self` drops, well past their
        // deadline.
        fail_all_pending(&mut self.pending, "session driver stopped");
        // Graceful socket teardown (sends a WebSocket Close frame), then publish
        // the terminal state. Bounded: a half-open exit can hang the flush, and
        // that would strand the JoinHandle and the state watchers behind it.
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.write.close()).await;
        match result {
            Ok(reason) => {
                info!(%reason, "session ended");
                let _ = self.state.send(SessionState::LoggedOff { reason });
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "session failed");
                let _ = self.state.send(SessionState::Failed {
                    error: e.to_string(),
                });
                Err(e)
            }
        }
    }

    /// Drive one connected session until it ends.
    async fn run_connected(&mut self) -> LoopExit {
        let mut next_heartbeat = Instant::now() + self.heartbeat;
        let mut next_sweep = Instant::now() + PENDING_SWEEP_INTERVAL;
        self.liveness.touch();
        loop {
            // Each arm borrows disjoint fields, so they can race in select!.
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        None => return LoopExit::Shutdown,
                        Some(Command::Notify { emsg, body, ack }) => {
                            let sent = send_frame(&mut self.write, self.steam_id, self.session_id, emsg, None, &body).await;
                            let failed = sent.is_err();
                            let _ = ack.send(sent);
                            if failed {
                                return LoopExit::Disconnected;
                            }
                        }
                        Some(Command::Logoff { ack }) => {
                            // Best-effort goodbye to Steam, then stop.
                            let _ = send_frame(&mut self.write, self.steam_id, self.session_id, EMSG_CLIENT_LOGOFF, None, &[]).await;
                            let _ = ack.send(());
                            return LoopExit::Shutdown;
                        }
                        Some(Command::Request { emsg, body, deadline, reply }) => {
                            let jobid = self.next_jobid;
                            self.next_jobid = self.next_jobid.checked_add(1).unwrap_or(1);
                            if send_frame(&mut self.write, self.steam_id, self.session_id, emsg, Some(jobid), &body).await.is_err() {
                                let _ = reply.send(Err(Error::WebSocket("session reconnecting".into())));
                                return LoopExit::Disconnected;
                            }
                            self.pending.insert(jobid, Pending { reply, deadline });
                        }
                    }
                }
                received = read_message(&mut self.read, &mut self.inbox, &self.liveness) => {
                    match received {
                        Ok(msg) if msg.emsg == EMSG_CLIENT_LOGGED_OFF => return classify_logoff(&msg),
                        Ok(msg) => dispatch(&mut self.pending, &self.events, &self.snapshots, msg),
                        Err(_) => return LoopExit::Disconnected,
                    }
                }
                () = tokio::time::sleep_until(next_heartbeat) => {
                    trace!("heartbeat");
                    if send_frame(&mut self.write, self.steam_id, self.session_id, EMSG_CLIENT_HEARTBEAT, None, &[]).await.is_err() {
                        return LoopExit::Disconnected;
                    }
                    // Steam never acks the heartbeat, so probe the socket too:
                    // the CM pongs this, and that lands on `liveness`.
                    if write_ping(&mut self.write).await.is_err() {
                        return LoopExit::Disconnected;
                    }
                    next_heartbeat = Instant::now() + self.heartbeat;
                }
                () = tokio::time::sleep_until(next_sweep) => {
                    next_sweep = Instant::now() + PENDING_SWEEP_INTERVAL;
                    // Silence only means death once our Ping probes have gone
                    // unanswered; an idle account sends no messages for minutes.
                    if read_idle_expired(&self.liveness, self.heartbeat) {
                        warn!(idle_secs = self.liveness.idle_for().as_secs(), "no inbound frames, reconnecting");
                        return LoopExit::Disconnected;
                    }
                    sweep_pending(&mut self.pending);
                }
            }
        }
    }

    /// Re-establish the connection with exponential backoff. Returns once
    /// reconnected, on a shutdown request, or `Err` if the token is rejected
    /// (retrying won't help).
    ///
    /// Both the backoff sleep and the connect race `commands`, so `logoff()` is
    /// answered mid-reconnect and a fully dropped session stops instead of
    /// reconnecting forever. Commands that arrive meanwhile are answered with
    /// "session reconnecting" rather than silently dropped.
    async fn reconnect(&mut self) -> Result<Reconnect> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            {
                let sleep = tokio::time::sleep(backoff);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        () = &mut sleep => break,
                        command = self.commands.recv() => {
                            if answer_while_down(command) {
                                return Ok(Reconnect::Stopped);
                            }
                        }
                    }
                }
            }
            debug!(backoff_secs = backoff.as_secs(), "reconnect attempt");
            let attempt = CmConnection::establish(
                &self.config.account_name,
                self.config.refresh_token.expose(),
                self.config.proxy.as_ref(),
            );
            tokio::pin!(attempt);
            let outcome = loop {
                tokio::select! {
                    result = &mut attempt => break result,
                    command = self.commands.recv() => {
                        if answer_while_down(command) {
                            return Ok(Reconnect::Stopped);
                        }
                    }
                }
            };
            match outcome {
                Ok((conn, logged)) => {
                    let (ws, steam_id, session_id, inbox, liveness) = conn.into_parts();
                    let (write, read) = ws.split();
                    self.write = write;
                    self.read = read;
                    self.inbox = inbox;
                    self.liveness = liveness;
                    self.steam_id = steam_id;
                    self.session_id = session_id;
                    self.heartbeat = logged.heartbeat_interval;
                    return Ok(Reconnect::Live);
                }
                // A rejected token won't fix itself; give up. A *retryable*
                // logon rejection (TryAnotherCM, ServiceUnavailable, …) is just
                // another failed attempt.
                Err(e @ Error::AuthRejected(_)) => {
                    error!(error = %e, "reconnect: token rejected, giving up");
                    return Err(e);
                }
                Err(e) => {
                    debug!(error = %e, "reconnect attempt failed, backing off");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

/// Fail every in-flight request with `reason`, used on each way out of a
/// connected loop: no reply can arrive on a socket that is going away, and a
/// caller left waiting would outlive its own deadline.
fn fail_all_pending(pending: &mut HashMap<u64, Pending>, reason: &'static str) {
    for (_jobid, entry) in pending.drain() {
        let _ = entry.reply.send(Err(Error::WebSocket(reason.into())));
    }
}

/// Inbound-silence budget for a session heartbeating at `heartbeat`, which is
/// also the WebSocket Ping probe interval.
fn idle_timeout(heartbeat: Duration) -> Duration {
    (heartbeat * IDLE_PROBE_MISSES).max(MIN_IDLE_TIMEOUT)
}

/// `true` when no frame of any kind has arrived for [`idle_timeout`]: that
/// many Ping probes unanswered, so the socket is dead rather than quiet.
fn read_idle_expired(liveness: &Liveness, heartbeat: Duration) -> bool {
    liveness.idle_for() >= idle_timeout(heartbeat)
}

/// How a [`SessionDriver::reconnect`] ended.
enum Reconnect {
    /// The socket is back up.
    Live,
    /// Shutdown was requested mid-reconnect (all handles dropped, or `logoff`).
    Stopped,
}

/// Answer a command that arrived while the socket is down. Returns `true` when
/// the driver must stop: all handles dropped, or an acked logoff.
fn answer_while_down(command: Option<Command>) -> bool {
    match command {
        None => true,
        Some(Command::Logoff { ack }) => {
            let _ = ack.send(());
            true
        }
        Some(Command::Request { reply, .. }) => {
            let _ = reply.send(Err(Error::WebSocket("session reconnecting".into())));
            false
        }
        Some(Command::Notify { ack, .. }) => {
            let _ = ack.send(Err(Error::WebSocket("session reconnecting".into())));
            false
        }
    }
}

/// Reap pending requests that timed out or whose caller went away, so a job
/// Steam never answers can't hang a caller or leak its slot forever.
fn sweep_pending(pending: &mut HashMap<u64, Pending>) {
    let now = Instant::now();
    pending.retain(|_jobid, entry| {
        if entry.reply.is_closed() {
            return false;
        }
        if entry.deadline <= now {
            // `retain` can't move the sender out, so signal through a swap.
            let (dead, _) = oneshot::channel();
            let reply = std::mem::replace(&mut entry.reply, dead);
            let _ = reply.send(Err(Error::Timeout("steam request")));
            return false;
        }
        true
    });
}

/// Build and send a frame with the session's routing header.
async fn send_frame(
    write: &mut SplitSink<SteamWebSocket, WsMessage>,
    steam_id: u64,
    session_id: i32,
    emsg: u32,
    jobid_source: Option<u64>,
    body: &[u8],
) -> Result<()> {
    let header = CMsgProtoBufHeader {
        steamid: Some(steam_id),
        client_sessionid: Some(session_id),
        jobid_source,
        ..Default::default()
    };
    write_frame(write, codec::encode_raw(emsg, &header, body)).await
}

/// Decide whether a `CMsgClientLoggedOff` is transient (heal by reconnecting) or
/// terminal (give up). Steam frequently evicts sessions with `TryAnotherCM` /
/// `ServiceUnavailable` under load — especially over flaky proxies — and the
/// right response is to re-establish, not to die.
fn classify_logoff(msg: &SteamMessage) -> LoopExit {
    let code = match CMsgClientLoggedOff::decode(msg.body.as_slice()) {
        // proto2 default for a missing `eresult` is Fail, not 0.
        Ok(decoded) => decoded.eresult.unwrap_or(eresult::FAIL),
        Err(e) => {
            warn!(error = %e, "server logged us off (undecodable body)");
            return LoopExit::LoggedOff("logged off by Steam (undecodable body)".to_string());
        }
    };
    match code {
        eresult::NO_CONNECTION | eresult::SERVICE_UNAVAILABLE | eresult::TRY_ANOTHER_CM => {
            warn!(
                eresult = code,
                "server logged us off (transient, reconnecting)"
            );
            LoopExit::Disconnected
        }
        other => {
            warn!(eresult = other, "server logged us off (terminal)");
            LoopExit::LoggedOff(format!("logged off by Steam (eresult {other})"))
        }
    }
}

/// Route an incoming message: to the waiting request if its `jobid_target`
/// matches a pending job, otherwise broadcast it to subscribers.
fn dispatch(
    pending: &mut HashMap<u64, Pending>,
    events: &broadcast::Sender<SteamMessage>,
    snapshots: &SnapshotCache,
    msg: SteamMessage,
) {
    // jobid_target defaults to u64::MAX (invalid) when there is no target.
    if let Some(jobid) = msg.header.jobid_target.filter(|&j| j != 0 && j != u64::MAX) {
        if let Some(entry) = pending.remove(&jobid) {
            let _ = entry.reply.send(Ok(msg));
            return;
        }
    }
    // Cache the first body seen per post-login-snapshot emsg this logon (the full
    // list precedes any delta), so `friends::request_*` can read it after the
    // fact instead of racing the push. `or_insert_with` makes it first-wins.
    if POST_LOGIN_SNAPSHOT_EMSGS.contains(&msg.emsg) {
        snapshots
            .lock()
            .expect("snapshot cache mutex poisoned")
            .entry(msg.emsg)
            .or_insert_with(|| msg.body.clone());
    }
    // Unsolicited, or no waiter — broadcast (ignored if there are no subscribers).
    let _ = events.send(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(emsg: u32, jobid_target: Option<u64>, body: Vec<u8>) -> SteamMessage {
        SteamMessage {
            emsg,
            header: CMsgProtoBufHeader {
                jobid_target,
                ..Default::default()
            },
            body,
        }
    }

    fn empty_snapshots() -> SnapshotCache {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn pending_entry(deadline: Instant) -> (Pending, oneshot::Receiver<Result<SteamMessage>>) {
        let (tx, rx) = oneshot::channel();
        (
            Pending {
                reply: tx,
                deadline,
            },
            rx,
        )
    }

    #[tokio::test]
    async fn dispatch_routes_reply_by_jobid() {
        let mut pending = HashMap::new();
        let (entry, rx) = pending_entry(Instant::now() + Duration::from_secs(60));
        pending.insert(42u64, entry);
        let (events, _keep) = broadcast::channel(8);

        dispatch(
            &mut pending,
            &events,
            &empty_snapshots(),
            msg(751, Some(42), vec![9]),
        );

        assert!(pending.is_empty(), "pending entry should be consumed");
        let routed = rx.await.expect("reply delivered").expect("ok message");
        assert_eq!(routed.body, vec![9]);
    }

    #[tokio::test]
    async fn dispatch_broadcasts_unsolicited() {
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);

        dispatch(
            &mut pending,
            &events,
            &empty_snapshots(),
            msg(5, None, vec![1, 2]),
        );

        let got = rx.try_recv().expect("event broadcast");
        assert_eq!(got.body, vec![1, 2]);
    }

    #[tokio::test]
    async fn dispatch_unmatched_jobid_is_broadcast() {
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);

        dispatch(
            &mut pending,
            &events,
            &empty_snapshots(),
            msg(751, Some(7), vec![3]),
        );

        assert_eq!(rx.try_recv().expect("event broadcast").body, vec![3]);
    }

    #[tokio::test]
    async fn dispatch_caches_first_snapshot_per_emsg() {
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let (events, _keep) = broadcast::channel(8);
        let snapshots = empty_snapshots();
        let friends_list = POST_LOGIN_SNAPSHOT_EMSGS[0];

        // The full snapshot arrives first and is cached…
        dispatch(
            &mut pending,
            &events,
            &snapshots,
            msg(friends_list, None, vec![1, 1]),
        );
        // …a later delta on the same emsg must NOT overwrite it (first-wins).
        dispatch(
            &mut pending,
            &events,
            &snapshots,
            msg(friends_list, None, vec![2, 2]),
        );

        let cached = snapshots.lock().unwrap().get(&friends_list).cloned();
        assert_eq!(cached, Some(vec![1, 1]), "first snapshot body wins");
    }

    #[tokio::test]
    async fn dispatch_does_not_cache_unlisted_emsg() {
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let (events, _keep) = broadcast::channel(8);
        let snapshots = empty_snapshots();

        // An arbitrary unsolicited emsg (persona state, say) is broadcast but
        // never cached — only the post-login snapshots are.
        dispatch(&mut pending, &events, &snapshots, msg(766, None, vec![7]));

        assert!(snapshots.lock().unwrap().is_empty());
    }

    fn logged_off(eresult: Option<i32>) -> SteamMessage {
        msg(
            EMSG_CLIENT_LOGGED_OFF,
            None,
            CMsgClientLoggedOff { eresult }.encode_to_vec(),
        )
    }

    #[test]
    fn classify_logoff_treats_transient_eresults_as_disconnect() {
        for er in [
            eresult::NO_CONNECTION,
            eresult::SERVICE_UNAVAILABLE,
            eresult::TRY_ANOTHER_CM,
        ] {
            assert!(
                matches!(
                    classify_logoff(&logged_off(Some(er))),
                    LoopExit::Disconnected
                ),
                "eresult {er} should reconnect"
            );
        }
    }

    #[test]
    fn classify_logoff_heals_the_load_balancing_logoff() {
        // TryAnotherCM is 48, not 42 (NoMatch), and it is the commonest
        // transient logoff Valve sends. 42 must stay terminal.
        assert!(matches!(
            classify_logoff(&logged_off(Some(48))),
            LoopExit::Disconnected
        ));
        assert!(matches!(
            classify_logoff(&logged_off(Some(42))),
            LoopExit::LoggedOff(_)
        ));
    }

    #[test]
    fn classify_logoff_treats_other_eresults_as_terminal() {
        // OK, LoggedInElsewhere, LogonSessionReplaced, Fail.
        for er in [1, 6, 34, eresult::FAIL] {
            assert!(
                matches!(
                    classify_logoff(&logged_off(Some(er))),
                    LoopExit::LoggedOff(_)
                ),
                "eresult {er} should be terminal"
            );
        }
    }

    #[test]
    fn classify_logoff_defaults_missing_eresult_to_fail() {
        // proto2 default is 2 (Fail), not 0, and Fail is terminal.
        match classify_logoff(&logged_off(None)) {
            LoopExit::LoggedOff(reason) => assert!(reason.contains('2'), "reason: {reason}"),
            _ => panic!("missing eresult should be terminal"),
        }
    }

    #[test]
    fn classify_logoff_reports_an_undecodable_body() {
        // 0xff is an invalid protobuf tag, so the body can't be an eresult 0.
        match classify_logoff(&msg(EMSG_CLIENT_LOGGED_OFF, None, vec![0xff, 0xff])) {
            LoopExit::LoggedOff(reason) => assert!(reason.contains("undecodable")),
            _ => panic!("undecodable body should be terminal"),
        }
    }

    fn reply_msg(eresult: Option<i32>, error_message: Option<&str>) -> SteamMessage {
        SteamMessage {
            emsg: 751,
            header: CMsgProtoBufHeader {
                eresult,
                error_message: error_message.map(str::to_string),
                ..Default::default()
            },
            body: Vec::new(),
        }
    }

    #[test]
    fn reply_without_eresult_is_ok() {
        assert!(check_reply_eresult(&reply_msg(None, None)).is_ok());
        assert!(check_reply_eresult(&reply_msg(Some(eresult::OK), None)).is_ok());
    }

    #[test]
    fn reply_with_non_ok_eresult_is_remote_error() {
        let err = check_reply_eresult(&reply_msg(Some(eresult::FAIL), None)).unwrap_err();
        match err {
            Error::Remote(text) => {
                assert!(text.contains("751"), "{text}");
                assert!(text.contains("eresult 2"), "{text}");
            }
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn reply_error_message_is_folded_in() {
        let err = check_reply_eresult(&reply_msg(Some(15), Some("access denied"))).unwrap_err();
        match err {
            Error::Remote(text) => assert!(text.contains("access denied"), "{text}"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sweep_fails_timed_out_requests() {
        let mut pending = HashMap::new();
        let (expired, expired_rx) = pending_entry(Instant::now() - Duration::from_secs(1));
        let (live, _live_rx) = pending_entry(Instant::now() + Duration::from_secs(60));
        pending.insert(1u64, expired);
        pending.insert(2u64, live);

        sweep_pending(&mut pending);

        assert!(!pending.contains_key(&1), "expired entry reaped");
        assert!(pending.contains_key(&2), "live entry kept");
        assert!(matches!(
            expired_rx.await.expect("sender not dropped"),
            Err(Error::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn sweep_drops_abandoned_requests() {
        let mut pending = HashMap::new();
        let (abandoned, rx) = pending_entry(Instant::now() + Duration::from_secs(60));
        pending.insert(1u64, abandoned);
        drop(rx); // caller's future went away

        sweep_pending(&mut pending);

        assert!(pending.is_empty(), "abandoned slot released");
    }

    #[tokio::test]
    async fn commands_during_reconnect_are_answered_not_dropped() {
        let (reply, reply_rx) = oneshot::channel();
        assert!(!answer_while_down(Some(Command::Request {
            emsg: 1,
            body: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(60),
            reply,
        })));
        assert!(reply_rx.await.expect("answered").is_err());

        let (ack, ack_rx) = oneshot::channel();
        assert!(!answer_while_down(Some(Command::Notify {
            emsg: 1,
            body: Vec::new(),
            ack,
        })));
        assert!(ack_rx.await.expect("answered").is_err());
    }

    #[tokio::test]
    async fn logoff_and_dropped_handles_stop_the_reconnect_loop() {
        let (ack, ack_rx) = oneshot::channel();
        assert!(answer_while_down(Some(Command::Logoff { ack })));
        assert!(ack_rx.await.is_ok(), "logoff acked mid-reconnect");
        assert!(answer_while_down(None), "dropped handles stop the driver");
    }

    #[tokio::test]
    async fn fail_all_pending_frees_callers_on_a_terminal_exit() {
        let mut pending = HashMap::new();
        let (entry, rx) = pending_entry(Instant::now() + Duration::from_secs(60));
        pending.insert(1u64, entry);

        fail_all_pending(&mut pending, "session driver stopped");

        assert!(pending.is_empty(), "slot released");
        match rx.await.expect("caller answered, not left hanging") {
            Err(Error::WebSocket(text)) => assert!(text.contains("driver stopped"), "{text}"),
            _ => panic!("expected a WebSocket error"),
        }
    }

    #[test]
    fn idle_timeout_follows_the_probe_interval_with_a_floor() {
        assert_eq!(
            idle_timeout(Duration::from_secs(30)),
            Duration::from_secs(30) * IDLE_PROBE_MISSES
        );
        // A short heartbeat can't shrink the budget below the floor.
        assert_eq!(idle_timeout(Duration::from_secs(1)), MIN_IDLE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_does_not_trip_while_probes_are_answered() {
        use crate::session::connection::read_frame;

        let liveness = Liveness::new();
        let heartbeat = Duration::from_secs(9);
        // Twenty probe intervals with no application traffic at all (an idle
        // farm account), but the CM pongs every probe, so the socket is alive.
        for _ in 0..20 {
            tokio::time::advance(heartbeat).await;
            let mut frames = futures_util::stream::iter(vec![Ok(WsMessage::Pong(Vec::new()))]);
            let _ = read_frame(&mut frames, &liveness).await;
            assert!(
                !read_idle_expired(&liveness, heartbeat),
                "an answered probe is proof of life"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_trips_when_no_frame_arrives_at_all() {
        let liveness = Liveness::new();
        let heartbeat = Duration::from_secs(9);

        tokio::time::advance(idle_timeout(heartbeat) - Duration::from_millis(1)).await;
        assert!(
            !read_idle_expired(&liveness, heartbeat),
            "still inside the budget"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            read_idle_expired(&liveness, heartbeat),
            "every probe unanswered means the socket is dead"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_is_enforced_caller_side() {
        // No driver ever answers; the caller's own timer must resolve it rather
        // than waiting on the 1s sweep.
        let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(7);

        let err = handle
            .request_with_timeout::<_, CMsgProtoBufHeader>(
                123,
                &CMsgProtoBufHeader::default(),
                Duration::from_millis(50),
            )
            .await
            .expect_err("nothing answers");

        assert!(matches!(err, Error::Timeout(_)), "{err:?}");
    }

    #[tokio::test]
    async fn request_ignoring_eresult_leaves_the_result_to_the_caller() {
        let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(7);
        // A fake driver answering every request with DuplicateRequest (29) in
        // the routing header: what `add_friend` reads as "already friends".
        let driver = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                if let Command::Request { reply, .. } = command {
                    let _ = reply.send(Ok(reply_msg(Some(29), None)));
                }
            }
        });

        let checked = handle
            .request::<_, CMsgProtoBufHeader>(1, &CMsgProtoBufHeader::default())
            .await;
        assert!(matches!(checked, Err(Error::Remote(_))), "header checked");

        let unchecked: CMsgProtoBufHeader = handle
            .request_ignoring_eresult(1, &CMsgProtoBufHeader::default())
            .await
            .expect("body handed back for the caller to judge");
        assert_eq!(unchecked, CMsgProtoBufHeader::default());

        drop(handle);
        driver.await.expect("fake driver");
    }

    #[tokio::test]
    async fn for_test_handle_queues_commands() {
        let (handle, mut commands, _events, snapshots) = SessionHandle::for_test(7);
        assert_eq!(handle.steam_id(), 7);

        let task = tokio::spawn(async move {
            handle
                .notify(123, &CMsgProtoBufHeader::default())
                .await
                .expect("notify acked");
        });

        match commands.recv().await.expect("command queued") {
            Command::Notify { emsg, ack, .. } => {
                assert_eq!(emsg, 123);
                ack.send(Ok(())).expect("ack delivered");
            }
            _ => panic!("expected a Notify"),
        }
        task.await.expect("notify task");
        assert!(snapshots.lock().unwrap().is_empty());
    }
}
