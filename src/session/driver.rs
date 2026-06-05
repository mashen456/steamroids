//! Background session driver and handle.
//!
//! [`spawn_session`] takes a logged-on [`CmConnection`] and moves it onto its
//! own task. The task owns the socket: it heartbeats, reads incoming messages,
//! and routes them — replies to in-flight requests by job id, everything else
//! to subscribers. Callers interact through the cloneable [`SessionHandle`].
//!
//! This is the concurrency model for fleets: many sessions, each a cheap task,
//! with `request`/`notify`/`subscribe` usable from anywhere a handle is cloned.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::codec::{self, SteamMessage};
use crate::proto::CMsgProtoBufHeader;
use crate::session::connection::{
    read_message, write_frame, CmConnection, EMSG_CLIENT_HEARTBEAT, EMSG_CLIENT_LOGGED_OFF,
};
use crate::transport::websocket::SteamWebSocket;
use crate::{Error, Result};

/// Outstanding-request and notification queue depth.
const COMMAND_CAPACITY: usize = 64;
/// Buffered unsolicited messages per subscriber before lag.
const EVENT_CAPACITY: usize = 256;

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
}

/// A cloneable handle to a running CM session.
///
/// Cloning is cheap; every clone talks to the same background task. The session
/// stays alive until **all** handles are dropped (or Steam logs it off).
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<SteamMessage>,
    steam_id: u64,
}

impl SessionHandle {
    /// The logged-on account's 64-bit `SteamID`.
    pub fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// Send `req` and await the reply Steam correlates by job id, decoding its
    /// body as `Resp`.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the driver has stopped, plus any transport /
    /// decode error.
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
}

/// Move `conn` (already logged on) onto a background task and return a handle
/// plus the task's [`JoinHandle`]. `heartbeat` is
/// [`LoggedOn::heartbeat_interval`](crate::session::LoggedOn::heartbeat_interval).
///
/// The task ends — `JoinHandle` resolves to `Ok(())` — when every
/// [`SessionHandle`] is dropped or Steam logs the session off; it resolves to
/// `Err` on a transport failure.
pub fn spawn_session(
    conn: CmConnection,
    heartbeat: Duration,
) -> (SessionHandle, JoinHandle<Result<()>>) {
    let (ws, steam_id, session_id, inbox) = conn.into_parts();
    let (write, read) = ws.split();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (evt_tx, _evt_rx) = broadcast::channel(EVENT_CAPACITY);

    let driver = SessionDriver {
        write,
        read,
        inbox,
        steam_id,
        session_id,
        heartbeat,
        next_jobid: 1,
        pending: HashMap::new(),
        commands: cmd_rx,
        events: evt_tx.clone(),
    };
    let join = tokio::spawn(driver.run());

    (
        SessionHandle {
            commands: cmd_tx,
            events: evt_tx,
            steam_id,
        },
        join,
    )
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
}

impl SessionDriver {
    async fn run(mut self) -> Result<()> {
        let mut next_heartbeat = Instant::now() + self.heartbeat;
        loop {
            let wait = next_heartbeat.saturating_duration_since(Instant::now());
            // Each arm borrows disjoint fields, so they can race in select!.
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        // All handles dropped — nothing left to drive.
                        None => return Ok(()),
                        Some(Command::Notify { emsg, body }) => {
                            send_frame(&mut self.write, self.steam_id, self.session_id, emsg, None, &body).await?;
                        }
                        Some(Command::Request { emsg, body, reply }) => {
                            let jobid = self.next_jobid;
                            self.next_jobid = self.next_jobid.checked_add(1).unwrap_or(1);
                            self.pending.insert(jobid, reply);
                            if let Err(e) = send_frame(&mut self.write, self.steam_id, self.session_id, emsg, Some(jobid), &body).await {
                                if let Some(tx) = self.pending.remove(&jobid) {
                                    let _ = tx.send(Err(e));
                                }
                            }
                        }
                    }
                }
                received = read_message(&mut self.read, &mut self.inbox) => {
                    let msg = received?;
                    if msg.emsg == EMSG_CLIENT_LOGGED_OFF {
                        return Ok(());
                    }
                    dispatch(&mut self.pending, &self.events, msg);
                }
                () = tokio::time::sleep(wait) => {
                    send_frame(&mut self.write, self.steam_id, self.session_id, EMSG_CLIENT_HEARTBEAT, None, &[]).await?;
                    next_heartbeat = Instant::now() + self.heartbeat;
                }
            }
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

        // No jobid_target → goes to subscribers.
        dispatch(&mut pending, &events, msg(5, None, vec![1, 2]));

        let got = rx.try_recv().expect("event broadcast");
        assert_eq!(got.body, vec![1, 2]);
    }

    #[tokio::test]
    async fn dispatch_unmatched_jobid_is_broadcast() {
        let mut pending: HashMap<u64, oneshot::Sender<Result<SteamMessage>>> = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);

        // jobid_target set but nothing pending → fall back to broadcast.
        dispatch(&mut pending, &events, msg(751, Some(7), vec![3]));

        assert_eq!(rx.try_recv().expect("event broadcast").body, vec![3]);
    }
}
