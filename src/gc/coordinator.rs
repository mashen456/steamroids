//! A Game Coordinator client multiplexed over a CM [`SessionHandle`].

use std::time::Duration;

use prost::Message;
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;

use crate::gc::envelope::{self, GcMessage, EMSG_CLIENT_FROM_GC, EMSG_CLIENT_TO_GC};
use crate::gc::{GC_CLIENT_HELLO, GC_CLIENT_WELCOME};
use crate::proto::c_msg_client_games_played::GamePlayed;
use crate::proto::gc::{CMsgClientHello, CMsgProtoBufHeader as GcHeader};
use crate::proto::CMsgClientGamesPlayed;
use crate::session::{SessionHandle, SessionState};
use crate::{Error, Result};

// k_EMsgClientGamesPlayed, from `enums_clientserver.proto`.
const EMSG_CLIENT_GAMES_PLAYED: u32 = 742;
/// Buffered GC messages per subscriber before lag.
const GC_EVENT_CAPACITY: usize = 64;
/// How often to re-send `ClientHello` until the GC welcomes us.
const HELLO_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// Cap on hello retries per (re)launch — ~`MAX × INTERVAL` before giving up, so a
/// license-less account doesn't poke the GC indefinitely.
const MAX_HELLO_ATTEMPTS: u32 = 20;

/// A handle to one app's Game Coordinator, riding on a CM session.
///
/// [`attach`](Self::attach) announces the app to Steam, prompts the GC's
/// welcome, and spawns a background pump that turns `k_EMsgClientFromGC` traffic
/// into [`GcMessage`]s. Cloning is cheap; every clone shares the one pump and
/// the same session. The pump re-announces the app automatically when the
/// underlying session reconnects, so the GC stays live across drops.
///
/// This type is app-agnostic. CS2 helpers live in [`crate::cs2`].
#[derive(Clone)]
pub struct GameCoordinator {
    session: SessionHandle,
    appid: u32,
    events: broadcast::Sender<GcMessage>,
    ready: watch::Receiver<bool>,
}

impl GameCoordinator {
    /// Attach to `appid`'s Game Coordinator over `session`.
    ///
    /// Tells Steam we're playing the app and sends a `ClientHello` (carrying
    /// `hello_version`, the app's GC protocol version) to prompt the welcome,
    /// then spawns the pump. Returns immediately — use
    /// [`wait_ready`](Self::wait_ready) to await the welcome before issuing
    /// requests. Most callers want the per-app wrapper (e.g.
    /// [`cs2::attach`](crate::cs2::attach)) which supplies the right version.
    ///
    /// `hello_version` matters: CS2's GC rejects a version-less hello with a
    /// fatal logon error, so pass the current app GC version (`0` only for GCs
    /// that don't check it).
    ///
    /// # Errors
    ///
    /// Propagates the initial launch send if the session has already stopped.
    pub async fn attach(session: SessionHandle, appid: u32, hello_version: u32) -> Result<Self> {
        // Subscribe *before* launching so the welcome can't race ahead of us.
        let events_in = session.subscribe();
        let state_in = session.watch_state();
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, ready_rx) = watch::channel(false);

        tokio::spawn(pump(
            appid,
            hello_version,
            session.clone(),
            events_in,
            state_in,
            events_out.clone(),
            ready_tx,
        ));

        // Kick the first launch eagerly so callers don't wait a state cycle.
        launch(&session, appid, hello_version).await?;

        Ok(Self {
            session,
            appid,
            events: events_out,
            ready: ready_rx,
        })
    }

    /// The app id this coordinator speaks to.
    #[must_use]
    pub fn appid(&self) -> u32 {
        self.appid
    }

    /// The underlying CM session handle.
    #[must_use]
    pub fn session(&self) -> &SessionHandle {
        &self.session
    }

    /// Subscribe to the raw stream of GC messages for this app. Late subscribers
    /// miss earlier messages.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<GcMessage> {
        self.events.subscribe()
    }

    /// Wait until the GC has welcomed us (or `deadline` elapses).
    ///
    /// Resolves immediately if the GC is already ready.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] if no welcome arrives in time, or [`Error::WebSocket`]
    /// if the session (and thus the pump) stopped first.
    pub async fn wait_ready(&self, deadline: Duration) -> Result<()> {
        let mut ready = self.ready.clone();
        if *ready.borrow() {
            return Ok(());
        }
        let wait = async {
            loop {
                if ready.changed().await.is_err() {
                    return Err(Error::WebSocket("GC pump stopped".into()));
                }
                if *ready.borrow() {
                    return Ok(());
                }
            }
        };
        timeout(deadline, wait)
            .await
            .map_err(|_| Error::Timeout("GC welcome"))?
    }

    /// Send a GC message without expecting a reply.
    ///
    /// `msgtype` is the bare GC message type; the protobuf flag is applied by
    /// the envelope.
    ///
    /// # Errors
    ///
    /// [`Error::WebSocket`] if the session has stopped.
    pub async fn send<M: Message>(&self, msgtype: u32, body: &M) -> Result<()> {
        let client = envelope::wrap(
            self.appid,
            msgtype,
            &GcHeader::default(),
            &body.encode_to_vec(),
        );
        self.session.notify(EMSG_CLIENT_TO_GC, &client).await
    }

    /// Send `body` as `send_type`, then await the next GC message of
    /// `response_type` and decode its body as `Resp`.
    ///
    /// Correlation is by **response type**, not job id: most app GCs (CS2
    /// included) don't echo job ids on game messages. Concurrent requests of the
    /// same `response_type` may therefore steal each other's replies — issue
    /// them one at a time, or match on the decoded body.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] if no matching reply arrives within `deadline`,
    /// [`Error::WebSocket`] if the pump stopped, or [`Error::Codec`] on a body
    /// that doesn't decode as `Resp`.
    pub async fn request<Req, Resp>(
        &self,
        send_type: u32,
        body: &Req,
        response_type: u32,
        deadline: Duration,
    ) -> Result<Resp>
    where
        Req: Message,
        Resp: Message + Default,
    {
        // Subscribe before sending so a fast reply can't slip past us.
        let mut rx = self.events.subscribe();
        self.send(send_type, body).await?;

        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(msg) if msg.msgtype == response_type => return Ok(msg),
                    // A non-matching message or a lagged slot: keep waiting.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(Error::WebSocket("GC pump stopped".into()))
                    }
                }
            }
        };
        let reply: GcMessage = timeout(deadline, wait)
            .await
            .map_err(|_| Error::Timeout("GC response"))??;
        Resp::decode(reply.body.as_slice())
            .map_err(|e| Error::Codec(format!("decode GC response: {e}")))
    }
}

/// Announce the app to Steam (so it routes this app's GC traffic to us) and send
/// the first `ClientHello`.
async fn launch(session: &SessionHandle, appid: u32, hello_version: u32) -> Result<()> {
    let games_played = CMsgClientGamesPlayed {
        games_played: vec![GamePlayed {
            // For a plain app the CGameID is just the app id in the low 32 bits.
            game_id: Some(u64::from(appid)),
            ..Default::default()
        }],
        ..Default::default()
    };
    session
        .notify(EMSG_CLIENT_GAMES_PLAYED, &games_played)
        .await?;
    send_hello(session, appid, hello_version).await
}

/// Send a single `ClientHello` to prompt the GC's `ClientWelcome`.
///
/// The GC often ignores the first hello (it isn't ready the instant we announce
/// the game), so the pump re-sends this on a timer until the welcome arrives —
/// mirroring how the official client behaves. `hello_version` is the app's GC
/// protocol version; CS2 rejects a `0`/absent version with a fatal logon error.
async fn send_hello(session: &SessionHandle, appid: u32, hello_version: u32) -> Result<()> {
    let body = CMsgClientHello {
        version: Some(hello_version),
        client_session_need: Some(0),
        client_launcher: Some(0),
        steam_launcher: Some(0),
        ..Default::default()
    };
    let hello = envelope::wrap(
        appid,
        GC_CLIENT_HELLO,
        &GcHeader::default(),
        &body.encode_to_vec(),
    );
    session.notify(EMSG_CLIENT_TO_GC, &hello).await
}

/// Background pump: relays `ClientFromGC` traffic into GC events, tracks
/// readiness, and re-launches the app when the session reconnects. Ends when the
/// session does.
async fn pump(
    appid: u32,
    hello_version: u32,
    session: SessionHandle,
    mut events_in: broadcast::Receiver<crate::codec::SteamMessage>,
    mut state_in: watch::Receiver<SessionState>,
    events_out: broadcast::Sender<GcMessage>,
    ready_tx: watch::Sender<bool>,
) {
    let mut ready = false;
    // Re-poke the GC with a hello until it welcomes us. The first tick fires one
    // interval in (launch already sent the initial hello).
    let mut hello = tokio::time::interval_at(
        tokio::time::Instant::now() + HELLO_RETRY_INTERVAL,
        HELLO_RETRY_INTERVAL,
    );
    let mut hello_attempts: u32 = 0;

    loop {
        tokio::select! {
            // React to session lifecycle: a reconnect resets GC state and needs
            // a fresh launch (Steam forgets we were "playing" on a new logon).
            changed = state_in.changed() => {
                if changed.is_err() {
                    break; // session gone for good
                }
                let ready_again = state_in.borrow().is_ready();
                ready = false;
                let _ = ready_tx.send(false);
                if ready_again {
                    hello_attempts = 0;
                    let _ = launch(&session, appid, hello_version).await;
                }
            }
            // Retry the hello until welcomed (bounded, so a license-less account
            // doesn't poke Steam forever).
            _ = hello.tick() => {
                if !ready && hello_attempts < MAX_HELLO_ATTEMPTS {
                    hello_attempts += 1;
                    let _ = send_hello(&session, appid, hello_version).await;
                }
            }
            received = events_in.recv() => {
                match received {
                    Ok(steam_msg) => {
                        if steam_msg.emsg != EMSG_CLIENT_FROM_GC {
                            continue;
                        }
                        match envelope::unwrap(&steam_msg) {
                            Ok(Some(gc_msg)) if gc_msg.appid == appid => {
                                if gc_msg.msgtype == GC_CLIENT_WELCOME {
                                    ready = true;
                                    let _ = ready_tx.send(true);
                                }
                                // Ignored if there are no subscribers.
                                let _ = events_out.send(gc_msg);
                            }
                            // Non-protobuf, other app, or a decode error: skip.
                            _ => {}
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
