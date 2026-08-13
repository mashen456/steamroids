//! A Game Coordinator client multiplexed over a CM [`SessionHandle`].

use std::collections::HashMap;
use std::time::Duration;

use prost::Message;
use tokio::sync::{broadcast, watch};
use tokio::time::{timeout, Instant, MissedTickBehavior};
use tracing::{debug, info, trace, warn, Instrument};

use crate::gc::envelope::{self, GcMessage, EMSG_CLIENT_FROM_GC, EMSG_CLIENT_TO_GC};
use crate::gc::{GC_CLIENT_CONNECTION_STATUS, GC_CLIENT_HELLO, GC_CLIENT_WELCOME};
use crate::proto::c_msg_client_games_played::GamePlayed;
use crate::proto::gc::{
    CMsgClientHello, CMsgClientWelcome, CMsgConnectionStatus, CMsgProtoBufHeader as GcHeader,
    GcConnectionStatus,
};
use crate::proto::CMsgClientGamesPlayed;
use crate::session::{SessionHandle, SessionState};
use crate::{Error, Result};

// k_EMsgClientGamesPlayed, from `enums_clientserver.proto`.
const EMSG_CLIENT_GAMES_PLAYED: u32 = 742;
/// Buffered GC messages per subscriber before lag.
const GC_EVENT_CAPACITY: usize = 64;
/// How often to re-send `ClientHello` until the GC welcomes us.
const HELLO_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// Cap on hello retries per readiness episode (~`MAX × INTERVAL` before giving
/// up), so a license-less account doesn't poke the GC indefinitely. Re-armed
/// whenever readiness is lost, so a GC that comes back is retried again.
const MAX_HELLO_ATTEMPTS: u32 = 20;

/// A handle to one app's Game Coordinator, riding on a CM session.
///
/// [`attach`](Self::attach) announces the app to Steam, prompts the GC's
/// welcome, and spawns a background pump that turns `k_EMsgClientFromGC` traffic
/// into [`GcMessage`]s. Cloning is cheap; every clone shares the one pump and
/// the same session. The pump re-announces the app automatically when the
/// underlying session reconnects, so the GC stays live across drops; readiness
/// also clears when the GC reports it has no session, which re-arms the hello
/// retries that win it back. Dropping the last clone stops the pump.
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
    /// Propagates the initial launch send: [`Error::WebSocket`] if the session
    /// has already stopped, is mid-reconnect, or the write failed.
    pub async fn attach(session: SessionHandle, appid: u32, hello_version: u32) -> Result<Self> {
        // Subscribe *before* launching so the welcome can't race ahead of us.
        let events_in = session.subscribe();
        let state_in = session.watch_state();
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, ready_rx) = watch::channel(false);

        debug!(appid, hello_version, "attaching GC");
        tokio::spawn(
            pump(
                appid,
                hello_version,
                session.clone(),
                events_in,
                state_in,
                events_out.clone(),
                ready_tx,
            )
            .instrument(tracing::debug_span!("gc", appid)),
        );

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
    /// [`Error::WebSocket`] if the session has stopped, is mid-reconnect, or
    /// the socket write itself failed. [`SessionHandle::notify`] awaits the
    /// write rather than just queueing it, so a dead socket surfaces here.
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
    /// same `response_type` would therefore steal each other's replies, so use
    /// [`request_matching`](Self::request_matching) to filter on the decoded
    /// body when more than one can be in flight.
    ///
    /// # Errors
    ///
    /// As [`Self::request_matching`].
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
        self.request_matching(send_type, body, response_type, deadline, |_: &Resp| true)
            .await
    }

    /// [`request`](Self::request) with a caller-supplied filter over the decoded
    /// reply.
    ///
    /// Every GC message of `response_type` is decoded as `Resp` and offered to
    /// `accept`; the first one it accepts is returned and rejected ones are
    /// discarded while the *same* `deadline` keeps running. Predicate on a field
    /// the GC echoes back from the request (an account id, a request id) and
    /// concurrent requests sharing a `response_type` stop stealing each other's
    /// replies.
    ///
    /// Waits for the GC's welcome first, inside `deadline`, so a request issued
    /// before the handshake completes isn't fired into a GC that hasn't
    /// acknowledged us.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] if the welcome or an accepted reply doesn't arrive
    /// within `deadline`, [`Error::WebSocket`] if the pump stopped or the
    /// request send failed (as [`Self::send`]), or [`Error::Codec`] on a body
    /// that doesn't decode as `Resp`.
    pub async fn request_matching<Req, Resp>(
        &self,
        send_type: u32,
        body: &Req,
        response_type: u32,
        deadline: Duration,
        accept: impl Fn(&Resp) -> bool,
    ) -> Result<Resp>
    where
        Req: Message,
        Resp: Message + Default,
    {
        // Readiness eats into the same budget the reply gets.
        let started = Instant::now();
        self.wait_ready(deadline).await?;
        let budget = deadline.saturating_sub(started.elapsed());

        // Subscribe before sending so a fast reply can't slip past us.
        let mut rx = self.events.subscribe();
        self.send(send_type, body).await?;

        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(msg) if msg.msgtype == response_type => {
                        let decoded = Resp::decode(msg.body.as_slice())
                            .map_err(|e| Error::Codec(format!("decode GC response: {e}")))?;
                        if accept(&decoded) {
                            return Ok(decoded);
                        }
                    }
                    // Wrong type, rejected by `accept`, or a lagged slot: wait on.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(Error::WebSocket("GC pump stopped".into()))
                    }
                }
            }
        };
        timeout(budget, wait)
            .await
            .map_err(|_| Error::Timeout("GC response"))?
    }

    /// **Test-only.** Build a coordinator with no pump behind it, returning the
    /// GC event sender and the readiness sender a test drives itself.
    ///
    /// Pair with [`SessionHandle::for_test`] to cover request builders offline.
    /// Not part of the supported API: it may change or vanish in any release.
    /// Compiled only under `cfg(test)` or the `test-seam` feature.
    #[cfg(any(test, feature = "test-seam"))]
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(
        session: SessionHandle,
        appid: u32,
    ) -> (Self, broadcast::Sender<GcMessage>, watch::Sender<bool>) {
        let (events, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, ready) = watch::channel(true);
        (
            Self {
                session,
                appid,
                events: events.clone(),
                ready,
            },
            events,
            ready_tx,
        )
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

/// Whether a `ClientConnectionStatus` body still reports a live GC session.
///
/// An absent status is proto2's `HAVE_SESSION` default; an undecodable body
/// counts as *no* session so readiness can't latch on a message we can't read.
fn has_gc_session(body: &[u8]) -> bool {
    match CMsgConnectionStatus::decode(body) {
        Ok(status) => status
            .status
            .is_none_or(|s| s == i32::from(GcConnectionStatus::HaveSession)),
        Err(_) => false,
    }
}

/// Outcome of extracting `game_data2` from a `ClientWelcome` body.
enum WelcomeGameData2 {
    /// The body didn't decode as a welcome at all: leave any existing cache
    /// entry alone rather than guess it away.
    Undecodable,
    /// The body decoded fine. `None` means it decoded but carried no
    /// `game_data2`, a full-inventory welcome that must still wipe a stale
    /// cache entry, the same policy [`so_objects_from_welcome`] follows for
    /// SO objects.
    Decoded(Option<Vec<u8>>),
}

/// A `ClientWelcome`'s `game_data2` payload.
///
/// `game_data2` is a generic `CMsgClientWelcome` field (any app's GC can use
/// it), so this stays app-agnostic: it never looks at what the bytes
/// contain. CS2's GC embeds a serialized
/// `CMsgGCCStrike15_v2_MatchmakingGC2ClientHello` here, decoded by
/// [`crate::cs2`].
fn game_data2_from_welcome(body: &[u8]) -> WelcomeGameData2 {
    match CMsgClientWelcome::decode(body) {
        Ok(welcome) => WelcomeGameData2::Decoded(welcome.game_data2),
        Err(_) => WelcomeGameData2::Undecodable,
    }
}

/// Flatten a `ClientWelcome`'s `outofdate_subscribed_caches` into
/// `type_id -> blobs`, or `None` if the body doesn't decode as a welcome at
/// all. A `type_id` absent from the welcome is simply absent from the map,
/// not an entry with an empty `Vec`.
fn so_objects_from_welcome(body: &[u8]) -> Option<HashMap<i32, Vec<Vec<u8>>>> {
    let welcome = CMsgClientWelcome::decode(body).ok()?;
    let mut objects: HashMap<i32, Vec<Vec<u8>>> = HashMap::new();
    for subscribed in welcome.outofdate_subscribed_caches {
        // drops subscribed.owner_soid, merges blobs across owners under one
        // type_id. only safe for per-account types (one owner per welcome).
        for obj in subscribed.objects {
            let Some(type_id) = obj.type_id else {
                continue;
            };
            objects.entry(type_id).or_default().extend(obj.object_data);
        }
    }
    Some(objects)
}

/// Background pump: relays `ClientFromGC` traffic into GC events, tracks
/// readiness, and re-launches the app when the session reconnects. Ends when the
/// session does, or when the last [`GameCoordinator`] clone is dropped (holding
/// the [`SessionHandle`] here would otherwise keep the session driver alive
/// forever). Every readiness loss (a CM state change, the GC reporting no
/// session, or the pump itself exiting) also clears `appid`'s cached SO
/// objects and cached penalty push, so [`SessionHandle::cached_so_objects`]
/// and [`SessionHandle::cached_gc_penalty`] can never answer from a welcome
/// that predates the current GC session.
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
    let mut hello =
        tokio::time::interval_at(Instant::now() + HELLO_RETRY_INTERVAL, HELLO_RETRY_INTERVAL);
    // A stalled pump must not fire a backlog of ticks and burn the budget.
    hello.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Budget is per readiness episode, not per session lifetime.
    let mut hello_attempts: u32 = 0;

    loop {
        tokio::select! {
            // Last GameCoordinator clone dropped: nothing to pump for, and the
            // session handle we hold must go with it.
            () = ready_tx.closed() => break,
            // React to session lifecycle: a reconnect resets GC state and needs
            // a fresh launch (Steam forgets we were "playing" on a new logon).
            changed = state_in.changed() => {
                if changed.is_err() {
                    break; // session gone for good
                }
                let ready_again = state_in.borrow().is_ready();
                ready = false;
                hello_attempts = 0;
                hello.reset();
                let _ = ready_tx.send(false);
                // cm session changed, so the prior welcome's SO caches can no
                // longer be vouched for until the gc re-welcomes us.
                session.replace_so_cache(appid, HashMap::new());
                session.set_cached_gc_penalty(appid, None);
                if ready_again {
                    debug!("re-announcing app to GC after reconnect");
                    let _ = launch(&session, appid, hello_version).await;
                }
            }
            // Retry the hello until welcomed (bounded, so a license-less account
            // doesn't poke Steam forever).
            _ = hello.tick() => {
                if !ready && hello_attempts < MAX_HELLO_ATTEMPTS {
                    hello_attempts += 1;
                    trace!(attempt = hello_attempts, "GC hello retry");
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
                                match gc_msg.msgtype {
                                    GC_CLIENT_WELCOME => {
                                        ready = true;
                                        // The welcome carries the account's
                                        // SO caches (Prime state among them);
                                        // an undecodable body just skips the
                                        // cache update, not readiness.
                                        // only reads outofdate_subscribed_caches, ignores field 4
                                        // uptodate_subscribed_caches. a welcome marking nothing
                                        // outofdate wipes the cache. not reachable today since our
                                        // hello sends no cache version for the gc to compare against.
                                        if let Some(objects) = so_objects_from_welcome(&gc_msg.body) {
                                            session.replace_so_cache(appid, objects);
                                        }
                                        // game_data2 frequently carries CS2's only
                                        // report of a penalty/cooldown: no standalone
                                        // 9110 hello follows. Cache it opaquely so a
                                        // late subscriber can't miss it.
                                        if let WelcomeGameData2::Decoded(game_data2) =
                                            game_data2_from_welcome(&gc_msg.body)
                                        {
                                            session.set_cached_gc_penalty(appid, game_data2);
                                        }
                                        info!("GC welcomed");
                                        let _ = ready_tx.send(true);
                                    }
                                    // GC went away: drop readiness and re-arm the
                                    // hello budget so the retries can recover it.
                                    GC_CLIENT_CONNECTION_STATUS => {
                                        if !has_gc_session(&gc_msg.body) {
                                            warn!("GC reports no session");
                                            ready = false;
                                            hello_attempts = 0;
                                            hello.reset();
                                            let _ = ready_tx.send(false);
                                            // gc dropped our session; the last welcome's
                                            // caches are stale until it welcomes us again.
                                            session.replace_so_cache(appid, HashMap::new());
                                            session.set_cached_gc_penalty(appid, None);
                                        }
                                    }
                                    _ => {}
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
    // pump is done answering for `appid`; a lingering cache would claim
    // freshness (see cached_so_objects's doc) it no longer has.
    session.replace_so_cache(appid, HashMap::new());
    session.set_cached_gc_penalty(appid, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gc::{c_msg_so_cache_subscribed::SubscribedType, CMsgSoCacheSubscribed};
    use crate::session::driver::Command;

    const APPID: u32 = 730;
    const SEND_TYPE: u32 = 9127;
    const RESPONSE_TYPE: u32 = 9128;

    fn status_body(status: GcConnectionStatus) -> Vec<u8> {
        CMsgConnectionStatus {
            status: Some(i32::from(status)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// A `RESPONSE_TYPE` reply carrying `version` as its only distinguishing field.
    fn reply(version: u32) -> GcMessage {
        GcMessage {
            appid: APPID,
            msgtype: RESPONSE_TYPE,
            header: GcHeader::default(),
            body: CMsgClientHello {
                version: Some(version),
                ..Default::default()
            }
            .encode_to_vec(),
        }
    }

    #[test]
    fn connection_status_without_a_session_is_not_ready() {
        for status in [
            GcConnectionStatus::GcGoingDown,
            GcConnectionStatus::NoSession,
            GcConnectionStatus::NoSessionInLogonQueue,
            GcConnectionStatus::NoSteam,
        ] {
            assert!(!has_gc_session(&status_body(status)), "{status:?}");
        }
    }

    #[test]
    fn connection_status_with_a_session_stays_ready() {
        assert!(has_gc_session(&status_body(
            GcConnectionStatus::HaveSession
        )));
        // Absent status is proto2's HAVE_SESSION default.
        assert!(has_gc_session(&[]));
    }

    #[test]
    fn undecodable_connection_status_is_not_a_session() {
        // 0xff is not a valid protobuf tag.
        assert!(!has_gc_session(&[0xff, 0xff]));
    }

    fn subscribed(type_id: i32, blobs: Vec<Vec<u8>>) -> CMsgSoCacheSubscribed {
        CMsgSoCacheSubscribed {
            objects: vec![SubscribedType {
                type_id: Some(type_id),
                object_data: blobs,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn so_objects_from_welcome_flattens_across_caches() {
        // Two subscribed caches both carrying type_id 7 must merge into one
        // entry, not overwrite each other.
        let welcome = CMsgClientWelcome {
            outofdate_subscribed_caches: vec![
                subscribed(7, vec![vec![1]]),
                subscribed(2, vec![vec![9]]),
                subscribed(7, vec![vec![2]]),
            ],
            ..Default::default()
        };
        let objects = so_objects_from_welcome(&welcome.encode_to_vec()).expect("decodes");
        assert_eq!(objects.get(&7), Some(&vec![vec![1], vec![2]]));
        assert_eq!(objects.get(&2), Some(&vec![vec![9]]));
    }

    #[test]
    fn so_objects_from_welcome_skips_a_blob_with_no_type_id() {
        let welcome = CMsgClientWelcome {
            outofdate_subscribed_caches: vec![CMsgSoCacheSubscribed {
                objects: vec![SubscribedType {
                    type_id: None,
                    object_data: vec![vec![1]],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let objects = so_objects_from_welcome(&welcome.encode_to_vec()).expect("decodes");
        assert!(objects.is_empty());
    }

    #[test]
    fn so_objects_from_welcome_is_none_on_garbage() {
        // 0xff is not a valid protobuf tag.
        assert!(so_objects_from_welcome(&[0xff, 0xff]).is_none());
    }

    #[tokio::test]
    async fn request_matching_skips_replies_the_predicate_rejects() {
        let (session, mut commands, _events_in, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APPID);

        let fake = tokio::spawn(async move {
            match commands.recv().await.expect("request sent") {
                Command::Notify { ack, .. } => ack.send(Ok(())).expect("ack delivered"),
                _ => panic!("expected a Notify"),
            }
            // Another request's reply first, then ours.
            let _ = replies.send(reply(1));
            let _ = replies.send(reply(2));
        });

        let got: CMsgClientHello = gc
            .request_matching(
                SEND_TYPE,
                &CMsgClientHello::default(),
                RESPONSE_TYPE,
                Duration::from_secs(5),
                |r: &CMsgClientHello| r.version == Some(2),
            )
            .await
            .expect("accepted reply");

        assert_eq!(got.version, Some(2));
        fake.await.expect("fake GC");
    }

    #[tokio::test]
    async fn request_is_gated_on_readiness() {
        let (session, mut commands, _events_in, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, ready) = GameCoordinator::for_test(session, APPID);
        ready.send(false).expect("clear readiness");

        let err = gc
            .request::<_, CMsgClientHello>(
                SEND_TYPE,
                &CMsgClientHello::default(),
                RESPONSE_TYPE,
                Duration::from_millis(50),
            )
            .await
            .expect_err("never welcomed");

        assert!(matches!(err, Error::Timeout("GC welcome")), "{err:?}");
        assert!(
            commands.try_recv().is_err(),
            "nothing sent to an unwelcomed GC"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_ready_returns_on_a_welcome_that_landed_before_the_call() {
        // The borrow() leg: readiness latched earlier is seen without waiting
        // on changed(), so even a zero deadline resolves.
        let (session, _commands, _events_in, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, ready) = GameCoordinator::for_test(session, APPID);
        ready.send(false).expect("clear readiness");
        ready.send(true).expect("welcome");

        gc.wait_ready(Duration::ZERO)
            .await
            .expect("already welcomed");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_ready_catches_a_welcome_that_lands_after_the_first_borrow() {
        // borrow() then changed(): changed() compares versions, so a welcome
        // sent while the waiter is parked can't be missed.
        let (session, _commands, _events_in, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, ready) = GameCoordinator::for_test(session, APPID);
        ready.send(false).expect("clear readiness");

        let waiting = tokio::spawn(async move { gc.wait_ready(Duration::from_secs(60)).await });
        // paused time only advances once every task is idle, so this sleep
        // returning proves the waiter is parked inside changed().
        tokio::time::sleep(Duration::from_secs(1)).await;
        ready.send(true).expect("welcome");

        waiting.await.expect("waiter").expect("welcomed");
    }

    #[tokio::test]
    async fn wait_ready_errors_when_the_pump_is_gone() {
        // The other half of "cannot deadlock": no welcome is ever coming, and
        // the dropped sender says so without waiting out the deadline.
        let (session, _commands, _events_in, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, ready) = GameCoordinator::for_test(session, APPID);
        ready.send(false).expect("clear readiness");
        drop(ready);

        let err = gc
            .wait_ready(Duration::from_secs(60))
            .await
            .expect_err("pump gone");
        assert!(matches!(err, Error::WebSocket(_)), "{err:?}");
    }

    #[tokio::test]
    async fn pump_releases_the_session_when_the_last_coordinator_drops() {
        let (session, mut commands, events_in, _snapshots) = SessionHandle::for_test(7);
        // A live state sender, so the pump can only leave via the drop signal.
        let (_state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id: 7 });
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, ready_rx) = watch::channel(false);

        let running = tokio::spawn(pump(
            APPID,
            1,
            session,
            events_in.subscribe(),
            state_rx,
            events_out,
            ready_tx,
        ));
        drop(ready_rx); // last GameCoordinator gone

        timeout(Duration::from_secs(5), running)
            .await
            .expect("pump exits")
            .expect("pump task");
        assert!(
            commands.recv().await.is_none(),
            "pump dropped its session handle"
        );
    }

    #[tokio::test]
    async fn pump_caches_so_objects_from_a_real_welcome() {
        use crate::codec::SteamMessage;
        use crate::proto::CMsgProtoBufHeader as SteamHeader;

        let (session, _commands, events_in, _snapshots) = SessionHandle::for_test(7);
        let readback = session.clone();
        let (_state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id: 7 });
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, mut ready_rx) = watch::channel(false);

        let running = tokio::spawn(pump(
            APPID,
            1,
            session,
            events_in.subscribe(),
            state_rx,
            events_out,
            ready_tx,
        ));

        // A real ClientWelcome, framed exactly as the CM would deliver it
        // (EMSG_CLIENT_FROM_GC -> CMsgGcClient -> the inner GC frame).
        let welcome = CMsgClientWelcome {
            outofdate_subscribed_caches: vec![subscribed(7, vec![vec![1, 2, 3]])],
            game_data2: Some(vec![9, 9, 9]),
            ..Default::default()
        };
        let client = envelope::wrap(
            APPID,
            GC_CLIENT_WELCOME,
            &GcHeader::default(),
            &welcome.encode_to_vec(),
        );
        events_in
            .send(SteamMessage {
                emsg: EMSG_CLIENT_FROM_GC,
                header: SteamHeader::default(),
                body: client.encode_to_vec(),
            })
            .expect("welcome delivered");

        // Readiness only flips after the welcome is processed, so waiting for
        // it also proves the cache write happened first.
        while !*ready_rx.borrow() {
            ready_rx.changed().await.expect("pump running");
        }
        assert_eq!(
            readback.cached_so_objects(APPID, 7),
            Some(vec![vec![1, 2, 3]])
        );
        assert_eq!(
            readback.cached_gc_penalty(APPID),
            Some(vec![9, 9, 9]),
            "game_data2 must be cached opaquely alongside the SO objects"
        );

        drop(ready_rx);
        timeout(Duration::from_secs(5), running)
            .await
            .expect("pump exits")
            .expect("pump task");
    }

    /// Push a real `ClientWelcome` carrying one SO object and a `game_data2`
    /// payload, then wait for the pump to process it (readiness flips only
    /// after the cache writes, so waiting for it also proves the writes
    /// happened first).
    async fn welcome_and_wait(
        events_in: &broadcast::Sender<crate::codec::SteamMessage>,
        ready_rx: &mut watch::Receiver<bool>,
    ) {
        use crate::codec::SteamMessage;
        use crate::proto::CMsgProtoBufHeader as SteamHeader;

        let welcome = CMsgClientWelcome {
            outofdate_subscribed_caches: vec![subscribed(7, vec![vec![1, 2, 3]])],
            game_data2: Some(vec![9, 9, 9]),
            ..Default::default()
        };
        let client = envelope::wrap(
            APPID,
            GC_CLIENT_WELCOME,
            &GcHeader::default(),
            &welcome.encode_to_vec(),
        );
        events_in
            .send(SteamMessage {
                emsg: EMSG_CLIENT_FROM_GC,
                header: SteamHeader::default(),
                body: client.encode_to_vec(),
            })
            .expect("welcome delivered");
        while !*ready_rx.borrow() {
            ready_rx.changed().await.expect("pump running");
        }
    }

    #[tokio::test]
    async fn pump_clears_so_cache_when_gc_reports_no_session() {
        use crate::codec::SteamMessage;
        use crate::proto::CMsgProtoBufHeader as SteamHeader;

        let (session, _commands, events_in, _snapshots) = SessionHandle::for_test(7);
        let readback = session.clone();
        let (_state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id: 7 });
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, mut ready_rx) = watch::channel(false);

        let running = tokio::spawn(pump(
            APPID,
            1,
            session,
            events_in.subscribe(),
            state_rx,
            events_out,
            ready_tx,
        ));

        welcome_and_wait(&events_in, &mut ready_rx).await;
        assert_eq!(
            readback.cached_so_objects(APPID, 7),
            Some(vec![vec![1, 2, 3]])
        );
        assert_eq!(readback.cached_gc_penalty(APPID), Some(vec![9, 9, 9]));

        // The GC drops our session without the CM connection itself dropping.
        let status = envelope::wrap(
            APPID,
            GC_CLIENT_CONNECTION_STATUS,
            &GcHeader::default(),
            &status_body(GcConnectionStatus::NoSession),
        );
        events_in
            .send(SteamMessage {
                emsg: EMSG_CLIENT_FROM_GC,
                header: SteamHeader::default(),
                body: status.encode_to_vec(),
            })
            .expect("status delivered");
        while *ready_rx.borrow() {
            ready_rx.changed().await.expect("pump running");
        }

        assert!(
            readback.cached_so_objects(APPID, 7).is_none(),
            "a stale SO cache must not survive the GC reporting no session"
        );
        assert!(
            readback.cached_gc_penalty(APPID).is_none(),
            "a stale penalty push must not survive the GC reporting no session"
        );

        drop(ready_rx);
        timeout(Duration::from_secs(5), running)
            .await
            .expect("pump exits")
            .expect("pump task");
    }

    #[tokio::test]
    async fn pump_clears_so_cache_when_the_cm_session_changes() {
        let (session, _commands, events_in, _snapshots) = SessionHandle::for_test(7);
        let readback = session.clone();
        let (state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id: 7 });
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, mut ready_rx) = watch::channel(false);

        let running = tokio::spawn(pump(
            APPID,
            1,
            session,
            events_in.subscribe(),
            state_rx,
            events_out,
            ready_tx,
        ));

        welcome_and_wait(&events_in, &mut ready_rx).await;
        assert_eq!(
            readback.cached_so_objects(APPID, 7),
            Some(vec![vec![1, 2, 3]])
        );
        assert_eq!(readback.cached_gc_penalty(APPID), Some(vec![9, 9, 9]));

        // The CM connection drops: a reconnect episode begins. `Connecting`
        // is not ready, so the pump won't try to re-launch and block on an
        // unanswered command.
        state_tx
            .send(SessionState::Connecting)
            .expect("state changed");
        while *ready_rx.borrow() {
            ready_rx.changed().await.expect("pump running");
        }

        assert!(
            readback.cached_so_objects(APPID, 7).is_none(),
            "a stale SO cache must not survive a CM reconnect"
        );
        assert!(
            readback.cached_gc_penalty(APPID).is_none(),
            "a stale penalty push must not survive a CM reconnect"
        );

        drop(ready_rx);
        timeout(Duration::from_secs(5), running)
            .await
            .expect("pump exits")
            .expect("pump task");
    }

    #[tokio::test]
    async fn pump_clears_so_cache_on_exit() {
        let (session, _commands, events_in, _snapshots) = SessionHandle::for_test(7);
        let readback = session.clone();
        let (_state_tx, state_rx) = watch::channel(SessionState::LoggedOn { steam_id: 7 });
        let (events_out, _keep) = broadcast::channel(GC_EVENT_CAPACITY);
        let (ready_tx, mut ready_rx) = watch::channel(false);

        let running = tokio::spawn(pump(
            APPID,
            1,
            session,
            events_in.subscribe(),
            state_rx,
            events_out,
            ready_tx,
        ));

        welcome_and_wait(&events_in, &mut ready_rx).await;
        assert!(readback.cached_so_objects(APPID, 7).is_some());
        assert!(readback.cached_gc_penalty(APPID).is_some());

        drop(ready_rx); // last GameCoordinator clone gone

        timeout(Duration::from_secs(5), running)
            .await
            .expect("pump exits")
            .expect("pump task");

        assert!(
            readback.cached_so_objects(APPID, 7).is_none(),
            "an exited pump must not leave a stale SO cache behind"
        );
        assert!(
            readback.cached_gc_penalty(APPID).is_none(),
            "an exited pump must not leave a stale penalty push behind"
        );
    }

    #[test]
    fn game_data2_from_welcome_extracts_the_payload() {
        let welcome = CMsgClientWelcome {
            game_data2: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert!(matches!(
            game_data2_from_welcome(&welcome.encode_to_vec()),
            WelcomeGameData2::Decoded(Some(bytes)) if bytes == vec![1, 2, 3]
        ));
    }

    #[test]
    fn game_data2_from_welcome_wipes_on_a_welcome_without_one() {
        // Decodes fine but carries no game_data2: Decoded(None), not
        // Undecodable, so the caller still wipes a stale cache entry
        // (full-inventory policy).
        let welcome = CMsgClientWelcome::default();
        assert!(matches!(
            game_data2_from_welcome(&welcome.encode_to_vec()),
            WelcomeGameData2::Decoded(None)
        ));
    }

    #[test]
    fn game_data2_from_welcome_is_undecodable_on_garbage() {
        // 0xff is not a valid protobuf tag: leaves a stale cache entry alone
        // rather than guessing it away.
        assert!(matches!(
            game_data2_from_welcome(&[0xff, 0xff]),
            WelcomeGameData2::Undecodable
        ));
    }
}
