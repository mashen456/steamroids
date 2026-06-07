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
use crate::proto::{CMsgClientLoggedOff, CMsgProtoBufHeader};
use crate::session::connection::{
    read_message, write_frame, CmConnection, LoggedOn, EMSG_CLIENT_HEARTBEAT,
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

// `EResult` values (from `steammessages_base.proto`) that make a server-side
// logoff *transient*: the session is gone, but reconnecting — through a rotating
// proxy, onto a fresh exit / CM — recovers it. Anything else is terminal.
const ERESULT_NO_CONNECTION: i32 = 3;
const ERESULT_SERVICE_UNAVAILABLE: i32 = 20;
const ERESULT_TRY_ANOTHER_CM: i32 = 42;

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

/// A request/notification queued from a [`SessionHandle`] to the driver.
enum Command {
    /// Send and correlate a reply by job id.
    Request {
        emsg: u32,
        body: Vec<u8>,
        reply: oneshot::Sender<Result<SteamMessage>>,
    },
    /// Fire-and-forget send (no reply expected).
    Notify { emsg: u32, body: Vec<u8> },
    /// Cleanly log off: tell Steam, then stop the driver.
    Logoff { ack: oneshot::Sender<()> },
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
    /// body as `Resp`.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver stopped or the session dropped the
    /// request mid-reconnect, plus any transport / decode error.
    pub async fn request<Req, Resp>(&self, emsg: u32, req: &Req) -> Result<Resp>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Request {
                emsg,
                body: req.encode_to_vec(),
                reply,
            })
            .await
            .map_err(|_| Error::WebSocket("session driver stopped".into()))?;
        let msg = rx
            .await
            .map_err(|_| Error::WebSocket("session driver dropped the request".into()))??;
        Resp::decode(msg.body.as_slice()).map_err(|e| Error::Codec(format!("decode response: {e}")))
    }

    /// Send `req` without expecting a reply.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver has stopped.
    pub async fn notify<Req: prost::Message>(&self, emsg: u32, req: &Req) -> Result<()> {
        self.commands
            .send(Command::Notify {
                emsg,
                body: req.encode_to_vec(),
            })
            .await
            .map_err(|_| Error::WebSocket("session driver stopped".into()))
    }

    /// Subscribe to unsolicited messages (notifications Steam pushes that aren't
    /// replies to a [`Self::request`]). Late subscribers miss earlier messages.
    pub fn subscribe(&self) -> broadcast::Receiver<SteamMessage> {
        self.events.subscribe()
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
    let (ws, _steam_id, session_id, inbox) = conn.into_parts();
    let (write, read) = ws.split();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (evt_tx, _evt_rx) = broadcast::channel(EVENT_CAPACITY);
    let (state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id });

    info!(steam_id, account = %config.account_name, "session established");
    let driver = SessionDriver {
        write,
        read,
        inbox,
        steam_id,
        session_id,
        heartbeat: logged.heartbeat_interval,
        next_jobid: 1,
        pending: HashMap::new(),
        commands: cmd_rx,
        events: evt_tx.clone(),
        state: state_tx,
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
    steam_id: u64,
    session_id: i32,
    heartbeat: Duration,
    next_jobid: u64,
    pending: HashMap<u64, oneshot::Sender<Result<SteamMessage>>>,
    commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<SteamMessage>,
    state: watch::Sender<SessionState>,
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
                    // In-flight requests can't be answered on a dead socket.
                    warn!(
                        pending = self.pending.len(),
                        "session disconnected, reconnecting"
                    );
                    self.fail_all_pending();
                    let _ = self.state.send(SessionState::Connecting);
                    match self.reconnect().await {
                        Ok(()) => {
                            info!(steam_id = self.steam_id, "session reconnected");
                            let _ = self.state.send(SessionState::LoggedOn {
                                steam_id: self.steam_id,
                            });
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
        };

        // Graceful socket teardown (sends a WebSocket Close frame), then publish
        // the terminal state.
        let _ = self.write.close().await;
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
        loop {
            let wait = next_heartbeat.saturating_duration_since(Instant::now());
            // Each arm borrows disjoint fields, so they can race in select!.
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        None => return LoopExit::Shutdown,
                        Some(Command::Notify { emsg, body }) => {
                            if send_frame(&mut self.write, self.steam_id, self.session_id, emsg, None, &body).await.is_err() {
                                return LoopExit::Disconnected;
                            }
                        }
                        Some(Command::Logoff { ack }) => {
                            // Best-effort goodbye to Steam, then stop.
                            let _ = send_frame(&mut self.write, self.steam_id, self.session_id, EMSG_CLIENT_LOGOFF, None, &[]).await;
                            let _ = ack.send(());
                            return LoopExit::Shutdown;
                        }
                        Some(Command::Request { emsg, body, reply }) => {
                            let jobid = self.next_jobid;
                            self.next_jobid = self.next_jobid.checked_add(1).unwrap_or(1);
                            if send_frame(&mut self.write, self.steam_id, self.session_id, emsg, Some(jobid), &body).await.is_err() {
                                let _ = reply.send(Err(Error::WebSocket("session reconnecting".into())));
                                return LoopExit::Disconnected;
                            }
                            self.pending.insert(jobid, reply);
                        }
                    }
                }
                received = read_message(&mut self.read, &mut self.inbox) => {
                    match received {
                        Ok(msg) if msg.emsg == EMSG_CLIENT_LOGGED_OFF => return classify_logoff(&msg),
                        Ok(msg) => dispatch(&mut self.pending, &self.events, msg),
                        Err(_) => return LoopExit::Disconnected,
                    }
                }
                () = tokio::time::sleep(wait) => {
                    trace!("heartbeat");
                    if send_frame(&mut self.write, self.steam_id, self.session_id, EMSG_CLIENT_HEARTBEAT, None, &[]).await.is_err() {
                        return LoopExit::Disconnected;
                    }
                    next_heartbeat = Instant::now() + self.heartbeat;
                }
            }
        }
    }

    /// Re-establish the connection with exponential backoff. Returns once
    /// reconnected, or `Err` if the token is rejected (retrying won't help).
    async fn reconnect(&mut self) -> Result<()> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            tokio::time::sleep(backoff).await;
            debug!(backoff_secs = backoff.as_secs(), "reconnect attempt");
            match CmConnection::establish(
                &self.config.account_name,
                self.config.refresh_token.expose(),
                self.config.proxy.as_ref(),
            )
            .await
            {
                Ok((conn, logged)) => {
                    let (ws, steam_id, session_id, inbox) = conn.into_parts();
                    let (write, read) = ws.split();
                    self.write = write;
                    self.read = read;
                    self.inbox = inbox;
                    self.steam_id = steam_id;
                    self.session_id = session_id;
                    self.heartbeat = logged.heartbeat_interval;
                    return Ok(());
                }
                // A rejected token won't fix itself — give up.
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

    /// Fail every in-flight request (used when the socket drops).
    fn fail_all_pending(&mut self) {
        for (_jobid, reply) in self.pending.drain() {
            let _ = reply.send(Err(Error::WebSocket("session reconnecting".into())));
        }
    }
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
    let eresult = CMsgClientLoggedOff::decode(msg.body.as_slice())
        .ok()
        .and_then(|m| m.eresult)
        .unwrap_or(0);
    match eresult {
        ERESULT_NO_CONNECTION | ERESULT_SERVICE_UNAVAILABLE | ERESULT_TRY_ANOTHER_CM => {
            warn!(eresult, "server logged us off (transient, reconnecting)");
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
    pending: &mut HashMap<u64, oneshot::Sender<Result<SteamMessage>>>,
    events: &broadcast::Sender<SteamMessage>,
    msg: SteamMessage,
) {
    // jobid_target defaults to u64::MAX (invalid) when there is no target.
    if let Some(jobid) = msg.header.jobid_target.filter(|&j| j != 0 && j != u64::MAX) {
        if let Some(reply) = pending.remove(&jobid) {
            let _ = reply.send(Ok(msg));
            return;
        }
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

    #[tokio::test]
    async fn dispatch_routes_reply_by_jobid() {
        let mut pending = HashMap::new();
        let (tx, rx) = oneshot::channel();
        pending.insert(42u64, tx);
        let (events, _keep) = broadcast::channel(8);

        dispatch(&mut pending, &events, msg(751, Some(42), vec![9]));

        assert!(pending.is_empty(), "pending entry should be consumed");
        let routed = rx.await.expect("reply delivered").expect("ok message");
        assert_eq!(routed.body, vec![9]);
    }

    #[tokio::test]
    async fn dispatch_broadcasts_unsolicited() {
        let mut pending: HashMap<u64, oneshot::Sender<Result<SteamMessage>>> = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);

        dispatch(&mut pending, &events, msg(5, None, vec![1, 2]));

        let got = rx.try_recv().expect("event broadcast");
        assert_eq!(got.body, vec![1, 2]);
    }

    #[tokio::test]
    async fn dispatch_unmatched_jobid_is_broadcast() {
        let mut pending: HashMap<u64, oneshot::Sender<Result<SteamMessage>>> = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);

        dispatch(&mut pending, &events, msg(751, Some(7), vec![3]));

        assert_eq!(rx.try_recv().expect("event broadcast").body, vec![3]);
    }
}
