//! A live Connection Manager (CM) session over a WebSocket.
//!
//! Wraps the [`connect_ws`] stream with the
//! [`codec`] framing: [`CmConnection::send`] and
//! [`CmConnection::recv`] deal in protobuf messages, and `recv` transparently
//! unpacks `Multi` batches (Steam coalesces several messages into one).
//!
//! [`CmConnection::logon`] performs the first real "logged into Steam" step:
//! send `CMsgClientLogon` with the refresh token from the
//! [`auth`](crate::auth) flow and await `CMsgClientLogonResponse`.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::read::GzDecoder;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use prost::Message as _;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info};

use crate::codec::{self, SteamMessage};
use crate::error::eresult;
use crate::proto::{
    CMsgClientLoggedOff, CMsgClientLogon, CMsgClientLogonResponse, CMsgMulti, CMsgProtoBufHeader,
};
use crate::transport::connect_ws;
use crate::transport::proxy::ProxyConfig;
use crate::transport::websocket::SteamWebSocket;
use crate::{Error, Result};

// EMsg values, from protos/steam/enums_clientserver.proto.
const EMSG_MULTI: u32 = 1;
pub(crate) const EMSG_CLIENT_HEARTBEAT: u32 = 703;
const EMSG_CLIENT_LOGON: u32 = 5514;
const EMSG_CLIENT_LOGON_RESPONSE: u32 = 751;
pub(crate) const EMSG_CLIENT_LOGGED_OFF: u32 = 757;

// Logon parameters mirroring an official client closely enough to be accepted.
const PROTOCOL_VERSION: u32 = 65580;
const CLIENT_PACKAGE_VERSION: u32 = 1771;
const CLIENT_OS_WINDOWS_10: u32 = 16;
/// Fallback heartbeat interval if Steam doesn't specify one.
const DEFAULT_HEARTBEAT_SECS: i32 = 9;

/// Hard cap on a decompressed `Multi` payload. Real Steam batches are orders of
/// magnitude smaller; the cap stops a hostile frame inflating into an OOM abort.
const MAX_MULTI_UNZIPPED: usize = 8 * 1024 * 1024;

/// Max nested `Multi` levels. Steam never nests beyond one; the bound stops a
/// hostile chain recursing into a stack overflow.
const MAX_MULTI_DEPTH: u32 = 4;

/// How many discovered CM servers to try before giving up a connect attempt.
const MAX_CONNECT_ATTEMPTS: usize = 5;

/// Per-server budget for the connect + logon handshake. A rotating proxy can
/// hand us a slow or silent exit; this bounds the wait so we abandon it and try
/// the next server (which opens a fresh connection — a new exit) instead of
/// stalling. Covers the full TCP/TLS/WS connect *and* the logon round-trip.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// Base `SteamID` for the *initial* logon header: universe Public, type
/// Individual, instance Desktop, account id 0. Steam replaces it with the real
/// `SteamID` in the logon response header.
const LOGON_HEADER_STEAMID: u64 = 0x0110_0001_0000_0000;

/// Details returned by a successful [`CmConnection::logon`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoggedOn {
    /// The account's real 64-bit `SteamID`, assigned by Steam.
    pub steam_id: u64,
    /// Session id Steam assigned; stamped into every subsequent message header.
    pub session_id: i32,
    /// How often to send `CMsgClientHeartBeat` to keep the session alive.
    pub heartbeat_interval: Duration,
}

/// Frame-level inbound-activity clock for one socket.
///
/// [`read_frame`] stamps it for **every** WebSocket frame that arrives (the
/// Ping / Pong control frames as much as Binary ones), so a watchdog can tell a
/// quiet peer from a dead one. Decoded Steam messages are not a liveness
/// signal: Steam never acks our heartbeats, and an idle account (no online
/// friends, no chat, no licence changes) can go minutes without any application
/// traffic at all. An answered WebSocket Ping is a liveness signal, and it
/// lands here.
///
/// Cheap to clone; every clone reads and writes the same timestamp.
#[derive(Debug, Clone)]
pub(crate) struct Liveness(Arc<Mutex<Instant>>);

impl Liveness {
    /// A clock started now.
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// Record that a frame just arrived.
    pub(crate) fn touch(&self) {
        *self.0.lock().expect("liveness mutex poisoned") = Instant::now();
    }

    /// How long since the last inbound frame.
    pub(crate) fn idle_for(&self) -> Duration {
        self.0.lock().expect("liveness mutex poisoned").elapsed()
    }
}

impl Default for Liveness {
    fn default() -> Self {
        Self::new()
    }
}

/// A live CM session over a WebSocket.
pub struct CmConnection {
    ws: SteamWebSocket,
    steam_id: u64,
    session_id: i32,
    /// Messages already pulled off the wire (e.g. unpacked from a `Multi`),
    /// waiting to be returned by [`Self::recv`].
    inbox: VecDeque<SteamMessage>,
    /// When this socket last had *any* frame arrive on it.
    liveness: Liveness,
}

impl CmConnection {
    /// Discover a CM, connect (trying servers in turn), and log on with a
    /// refresh token — the whole "get me a live, logged-on session" flow.
    ///
    /// Transport failures and [`Error::LogonRetryable`] fall through to the next
    /// server; an auth rejection (bad / expired token) stops immediately since
    /// retrying won't help.
    ///
    /// # Errors
    ///
    /// The last connect error if every server fails, [`Error::AuthRejected`] on
    /// a rejected token, or a discovery error.
    pub async fn establish(
        account_name: &str,
        refresh_token: &str,
        proxy: Option<&ProxyConfig>,
    ) -> Result<(Self, LoggedOn)> {
        let servers = crate::session::discover_cm_servers(proxy).await?;
        let mut last_err = None;
        for server in servers.iter().take(MAX_CONNECT_ATTEMPTS) {
            // Bound each server attempt so a slow/silent proxy exit is skipped
            // rather than stalling the whole connect. Each attempt opens a fresh
            // connection, so the next one routes through a new rotating exit.
            debug!(server = %server.endpoint, "cm connect attempt");
            let attempt = async {
                let mut conn = Self::connect(&server.ws_url(), proxy).await?;
                let logged = conn.logon(account_name, refresh_token).await?;
                Ok::<_, Error>((conn, logged))
            };
            match timeout(CONNECT_ATTEMPT_TIMEOUT, attempt).await {
                Ok(Ok((conn, logged))) => {
                    info!(steam_id = logged.steam_id, server = %server.endpoint, "cm logged on");
                    return Ok((conn, logged));
                }
                // A rejected token won't improve on another server — stop now.
                Ok(Err(e @ Error::AuthRejected(_))) => return Err(e),
                Ok(Err(e)) => {
                    debug!(server = %server.endpoint, error = %e, "cm connect attempt failed");
                    last_err = Some(e);
                }
                Err(_) => {
                    debug!(server = %server.endpoint, "cm connect attempt timed out");
                    last_err = Some(Error::Timeout("cm connect attempt"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Network("no CM servers to connect to".into())))
    }

    /// Open a CM connection to `ws_url` (from
    /// [`CmServer::ws_url`](crate::session::CmServer::ws_url)), optionally
    /// through a proxy.
    pub async fn connect(ws_url: &str, proxy: Option<&ProxyConfig>) -> Result<Self> {
        let ws = connect_ws(ws_url, proxy).await?;
        Ok(Self {
            ws,
            steam_id: LOGON_HEADER_STEAMID,
            session_id: 0,
            inbox: VecDeque::new(),
            liveness: Liveness::new(),
        })
    }

    /// The `SteamID` assigned after a successful [`Self::logon`] (a placeholder
    /// before then).
    pub fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// Send a protobuf message, stamping the current `SteamID` / session id into
    /// the header.
    pub async fn send<M: prost::Message>(&mut self, emsg: u32, body: &M) -> Result<()> {
        let header = CMsgProtoBufHeader {
            steamid: Some(self.steam_id),
            client_sessionid: Some(self.session_id),
            ..Default::default()
        };
        let frame = codec::encode(emsg, &header, body);
        write_frame(&mut self.ws, frame).await
    }

    /// Receive the next Steam message, transparently unpacking `Multi` batches.
    pub async fn recv(&mut self) -> Result<SteamMessage> {
        read_message(&mut self.ws, &mut self.inbox, &self.liveness).await
    }

    /// The session id Steam assigned at logon (0 before logon).
    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    /// Consume the connection into the pieces a background driver needs: the
    /// WebSocket stream, the logged-on `SteamID` / session id, any messages
    /// already buffered from a `Multi`, and the socket's [`Liveness`] clock.
    pub(crate) fn into_parts(self) -> (SteamWebSocket, u64, i32, VecDeque<SteamMessage>, Liveness) {
        (
            self.ws,
            self.steam_id,
            self.session_id,
            self.inbox,
            self.liveness,
        )
    }

    /// Log in over the CM using `refresh_token` (the token from the
    /// [`auth`](crate::auth) `WebAPI` flow). Drives `CMsgClientLogon` →
    /// `CMsgClientLogonResponse`.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRejected`] if Steam returns a *terminal* non-OK `EResult`
    /// (bad token, Steam Guard, banned account),
    /// [`Error::LogonRetryable`] for a transient one (`TryAnotherCM`,
    /// `ServiceUnavailable`, rate limiting, …); transport / codec errors
    /// propagate as-is.
    pub async fn logon(&mut self, account_name: &str, refresh_token: &str) -> Result<LoggedOn> {
        let logon = CMsgClientLogon {
            protocol_version: Some(PROTOCOL_VERSION),
            client_package_version: Some(CLIENT_PACKAGE_VERSION),
            client_language: Some("english".to_string()),
            client_os_type: Some(CLIENT_OS_WINDOWS_10),
            // Persistent logon: the refresh token is meant to be reused across
            // sessions, so don't let Steam treat it as a one-shot session token.
            should_remember_password: Some(true),
            account_name: Some(account_name.to_string()),
            access_token: Some(refresh_token.to_string()),
            supports_rate_limit_response: Some(true),
            ..Default::default()
        };
        self.send(EMSG_CLIENT_LOGON, &logon).await?;

        loop {
            let msg = self.recv().await?;
            match msg.emsg {
                EMSG_CLIENT_LOGON_RESPONSE => {
                    let resp = CMsgClientLogonResponse::decode(msg.body.as_slice())
                        .map_err(|e| Error::Codec(format!("decode logon response: {e}")))?;
                    let eresult = resp.eresult.unwrap_or(eresult::FAIL);
                    if eresult != eresult::OK {
                        return Err(logon_rejection(
                            eresult,
                            format!("CM logon eresult {eresult}"),
                        ));
                    }
                    // Steam assigns our real `SteamID` + session in the response header.
                    if let Some(sid) = msg.header.steamid {
                        self.steam_id = sid;
                    }
                    if let Some(sess) = msg.header.client_sessionid {
                        self.session_id = sess;
                    }
                    let hb = resp
                        .heartbeat_seconds
                        .unwrap_or(DEFAULT_HEARTBEAT_SECS)
                        .max(1);
                    return Ok(LoggedOn {
                        steam_id: self.steam_id,
                        session_id: self.session_id,
                        #[allow(clippy::cast_sign_loss)]
                        heartbeat_interval: Duration::from_secs(hb as u64),
                    });
                }
                EMSG_CLIENT_LOGGED_OFF => {
                    let eresult = CMsgClientLoggedOff::decode(msg.body.as_slice())
                        .map_or(eresult::FAIL, |m| m.eresult.unwrap_or(eresult::FAIL));
                    return Err(logon_rejection(
                        eresult,
                        format!("CM logged us off during logon (eresult {eresult})"),
                    ));
                }
                // Ignore anything else (server lists, etc.) until the response.
                _ => {}
            }
        }
    }
}

/// Classify a rejected logon `EResult` into a retryable or terminal error.
///
/// Only the codes we can positively name as transient retry; everything else
/// (bad token, Steam Guard, banned / disabled account, and anything
/// unrecognised) stays terminal so a genuinely broken account fails loudly.
pub(crate) fn logon_rejection(eresult: i32, reason: String) -> Error {
    if logon_eresult_retryable(eresult) {
        Error::LogonRetryable(reason)
    } else {
        Error::AuthRejected(reason)
    }
}

/// `true` when a logon `EResult` is worth retrying on a fresh connection.
///
/// These are the codes that make a rejection *transient*: the account is fine
/// and a fresh connection (new proxy exit / CM) is expected to work.
pub(crate) fn logon_eresult_retryable(code: i32) -> bool {
    matches!(
        code,
        eresult::NO_CONNECTION
            | eresult::BUSY
            | eresult::TIMEOUT
            | eresult::SERVICE_UNAVAILABLE
            | eresult::TRY_ANOTHER_CM
            | eresult::RATE_LIMIT_EXCEEDED
    )
}

/// Read the next binary WebSocket frame from `read`, skipping control frames
/// and stamping `liveness` for every frame that arrives, control frames
/// included. Generic over the stream so both [`CmConnection`] (full socket) and
/// the background driver (a split read half) can use it.
pub(crate) async fn read_frame<S>(read: &mut S, liveness: &Liveness) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<WsMessage, WsError>> + Unpin,
{
    loop {
        let ws_msg = read
            .next()
            .await
            .ok_or_else(|| Error::WebSocket("connection closed".into()))??;
        // any frame proves the peer is alive, incl. a Pong answering our probe
        liveness.touch();
        match ws_msg {
            WsMessage::Binary(data) => return Ok(data),
            WsMessage::Close(_) => {
                return Err(Error::WebSocket("server closed the connection".into()))
            }
            // tungstenite answers pings itself; ignore control/empty frames.
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
            WsMessage::Text(_) => {
                return Err(Error::WebSocket("unexpected text frame from CM".into()))
            }
        }
    }
}

/// Read the next decoded Steam message, draining `inbox` first and unpacking
/// `Multi` batches into it. Skips legacy non-protobuf messages.
pub(crate) async fn read_message<S>(
    read: &mut S,
    inbox: &mut VecDeque<SteamMessage>,
    liveness: &Liveness,
) -> Result<SteamMessage>
where
    S: Stream<Item = std::result::Result<WsMessage, WsError>> + Unpin,
{
    loop {
        if let Some(msg) = inbox.pop_front() {
            return Ok(msg);
        }
        let frame = read_frame(read, liveness).await?;
        match codec::try_decode(&frame)? {
            Some(msg) if msg.emsg == EMSG_MULTI => inbox.extend(decode_multi(&msg.body, 0)?),
            Some(msg) => return Ok(msg),
            None => {}
        }
    }
}

/// Write one already-encoded frame to `write`.
pub(crate) async fn write_frame<Si>(write: &mut Si, frame: Vec<u8>) -> Result<()>
where
    Si: Sink<WsMessage, Error = WsError> + Unpin,
{
    write.send(WsMessage::Binary(frame)).await?;
    Ok(())
}

/// Send a WebSocket Ping probe.
///
/// This is the *active* half of the driver's read-idle watchdog: a live peer
/// must answer a Ping with a Pong (RFC 6455 §5.5.2), and that Pong stamps the
/// socket's [`Liveness`], so silence becomes answerable instead of ambiguous.
/// Verified against a live CM (`cmp1-lhr1.steamserver.net`): it pongs every
/// probe, immediately, at the 9s heartbeat cadence.
pub(crate) async fn write_ping<Si>(write: &mut Si) -> Result<()>
where
    Si: Sink<WsMessage, Error = WsError> + Unpin,
{
    write.send(WsMessage::Ping(Vec::new())).await?;
    Ok(())
}

/// Split a `CMsgMulti` body into its embedded messages. Each is a u32-LE
/// length prefix followed by a full codec frame; nested `Multi`s are flattened.
///
/// `depth` is the current nesting level, bounded by [`MAX_MULTI_DEPTH`].
fn decode_multi(body: &[u8], depth: u32) -> Result<Vec<SteamMessage>> {
    if depth > MAX_MULTI_DEPTH {
        return Err(Error::Codec(format!(
            "Multi nested deeper than {MAX_MULTI_DEPTH}"
        )));
    }
    let multi = CMsgMulti::decode(body).map_err(|e| Error::Codec(format!("decode Multi: {e}")))?;
    let raw = multi.message_body.unwrap_or_default();
    // A non-zero `size_unzipped` means `message_body` is gzip-compressed. The
    // size is wire-supplied, so reject an absurd one before it reaches an
    // allocation.
    let payload = match multi.size_unzipped.unwrap_or(0) as usize {
        0 => raw,
        size if size > MAX_MULTI_UNZIPPED => {
            return Err(Error::Codec(format!(
                "Multi size_unzipped {size} over {MAX_MULTI_UNZIPPED} cap"
            )))
        }
        size => gunzip(&raw, size)?,
    };

    let mut out = Vec::new();
    let mut cur = &payload[..];
    while cur.len() >= 4 {
        let len = u32::from_le_bytes(cur[..4].try_into().unwrap()) as usize;
        cur = &cur[4..];
        if cur.len() < len {
            return Err(Error::Codec("truncated message inside Multi".into()));
        }
        match codec::try_decode(&cur[..len])? {
            Some(sub) if sub.emsg == EMSG_MULTI => out.extend(decode_multi(&sub.body, depth + 1)?),
            Some(sub) => out.push(sub),
            // Skip legacy non-protobuf sub-messages.
            None => {}
        }
        cur = &cur[len..];
    }
    Ok(out)
}

/// Gzip-decompress a `Multi` body, pre-sizing the buffer to `expected_len` and
/// refusing to inflate past [`MAX_MULTI_UNZIPPED`] (decompression bomb).
fn gunzip(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(expected_len.min(MAX_MULTI_UNZIPPED));
    // clamp inflate: one byte past the cap is enough to prove it overflowed
    GzDecoder::new(data)
        .take((MAX_MULTI_UNZIPPED + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Codec(format!("gunzip Multi: {e}")))?;
    if buf.len() > MAX_MULTI_UNZIPPED {
        return Err(Error::Codec(format!(
            "Multi inflated past {MAX_MULTI_UNZIPPED} cap"
        )));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(emsg: u32, steamid: u64) -> Vec<u8> {
        codec::encode(
            emsg,
            &CMsgProtoBufHeader {
                steamid: Some(steamid),
                ..Default::default()
            },
            &CMsgProtoBufHeader::default(),
        )
    }

    fn multi_body(messages: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = Vec::new();
        for m in messages {
            #[allow(clippy::cast_possible_truncation)]
            payload.extend_from_slice(&(m.len() as u32).to_le_bytes());
            payload.extend_from_slice(m);
        }
        CMsgMulti {
            size_unzipped: Some(0),
            message_body: Some(payload),
        }
        .encode_to_vec()
    }

    /// Wrap `body` (a `CMsgMulti` encoding) in `levels` further `Multi` layers.
    fn nest_multi(levels: u32) -> Vec<u8> {
        let mut body = multi_body(&[framed(703, 1)]);
        for _ in 0..levels {
            let inner = codec::encode_raw(EMSG_MULTI, &CMsgProtoBufHeader::default(), &body);
            body = multi_body(&[inner]);
        }
        body
    }

    #[test]
    fn decode_multi_splits_embedded_messages() {
        let body = multi_body(&[framed(703, 1), framed(751, 2)]);
        let msgs = decode_multi(&body, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].emsg, 703);
        assert_eq!(msgs[0].header.steamid, Some(1));
        assert_eq!(msgs[1].emsg, 751);
        assert_eq!(msgs[1].header.steamid, Some(2));
    }

    #[test]
    fn decode_multi_flattens_nested_multi() {
        let inner = multi_body(&[framed(751, 9)]);
        let inner_framed = codec::encode(
            EMSG_MULTI,
            &CMsgProtoBufHeader::default(),
            &CMsgMulti::decode(inner.as_slice()).unwrap(),
        );
        let body = multi_body(&[framed(703, 1), inner_framed]);
        let msgs = decode_multi(&body, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].emsg, 751);
        assert_eq!(msgs[1].header.steamid, Some(9));
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn decode_multi_handles_gzip() {
        let mut payload = Vec::new();
        for m in [framed(703, 1), framed(751, 2)] {
            #[allow(clippy::cast_possible_truncation)]
            payload.extend_from_slice(&(m.len() as u32).to_le_bytes());
            payload.extend_from_slice(&m);
        }
        #[allow(clippy::cast_possible_truncation)]
        let body = CMsgMulti {
            size_unzipped: Some(payload.len() as u32),
            message_body: Some(gzip(&payload)),
        }
        .encode_to_vec();

        let msgs = decode_multi(&body, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].emsg, 703);
        assert_eq!(msgs[1].emsg, 751);
    }

    #[test]
    fn decode_multi_rejects_invalid_gzip() {
        let body = CMsgMulti {
            size_unzipped: Some(4096),
            message_body: Some(vec![1, 2, 3]), // not valid gzip
        }
        .encode_to_vec();
        assert!(matches!(
            decode_multi(&body, 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[test]
    fn decode_multi_rejects_truncated_embedded_message() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u32.to_le_bytes()); // claims 100 bytes
        payload.extend_from_slice(&[0u8; 10]); // but only 10 follow
        let body = CMsgMulti {
            size_unzipped: Some(0),
            message_body: Some(payload),
        }
        .encode_to_vec();
        assert!(matches!(
            decode_multi(&body, 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[test]
    fn decode_multi_rejects_decompression_bomb() {
        // Small frame, huge inflated output: `size_unzipped` stays under the cap
        // so only the inflate limit can stop it.
        let bomb = gzip(&vec![0u8; MAX_MULTI_UNZIPPED + 4096]);
        assert!(bomb.len() < 64 * 1024, "bomb frame should be tiny");
        let body = CMsgMulti {
            size_unzipped: Some(1),
            message_body: Some(bomb),
        }
        .encode_to_vec();
        assert!(matches!(
            decode_multi(&body, 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[test]
    fn decode_multi_rejects_absurd_size_unzipped() {
        let body = CMsgMulti {
            size_unzipped: Some(u32::MAX),
            message_body: Some(vec![1, 2, 3]),
        }
        .encode_to_vec();
        assert!(matches!(
            decode_multi(&body, 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[test]
    fn decode_multi_rejects_deep_nesting() {
        let body = nest_multi(MAX_MULTI_DEPTH + 2);
        assert!(matches!(
            decode_multi(&body, 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[test]
    fn decode_multi_allows_nesting_within_the_cap() {
        let msgs = decode_multi(&nest_multi(MAX_MULTI_DEPTH - 1), 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].emsg, 703);
    }

    #[test]
    fn decode_multi_allows_exactly_the_cap() {
        // Exact boundary: `nest_multi(n)` recurses to depth `n`, so the deepest
        // *accepted* chain is MAX_MULTI_DEPTH itself. Pins `>` against `>=`.
        let msgs = decode_multi(&nest_multi(MAX_MULTI_DEPTH), 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].emsg, 703);
    }

    #[test]
    fn decode_multi_rejects_one_past_the_cap() {
        assert!(matches!(
            decode_multi(&nest_multi(MAX_MULTI_DEPTH + 1), 0).unwrap_err(),
            Error::Codec(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn read_frame_records_control_frames_as_activity() {
        // A CM answering our Ping probes sends Pongs and nothing else; those
        // must count as inbound activity or the watchdog kills a live socket.
        let liveness = Liveness::new();
        tokio::time::advance(Duration::from_secs(300)).await;
        let mut frames = futures_util::stream::iter(vec![
            Ok(WsMessage::Pong(Vec::new())),
            Ok(WsMessage::Ping(Vec::new())),
            Ok(WsMessage::Binary(vec![1, 2, 3])),
        ]);

        let frame = read_frame(&mut frames, &liveness).await.unwrap();

        assert_eq!(frame, vec![1, 2, 3]);
        assert_eq!(liveness.idle_for(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn read_frame_touches_liveness_on_a_pong_alone() {
        let liveness = Liveness::new();
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(liveness.idle_for() >= Duration::from_secs(120));

        // The Pong is the only frame: read_frame then runs out of stream and
        // errors, but the liveness stamp must already have landed.
        let mut frames = futures_util::stream::iter(vec![Ok(WsMessage::Pong(Vec::new()))]);
        assert!(read_frame(&mut frames, &liveness).await.is_err());

        assert_eq!(liveness.idle_for(), Duration::ZERO);
    }

    #[test]
    fn transient_logon_eresults_are_retryable() {
        for er in [
            eresult::NO_CONNECTION,
            eresult::BUSY,
            eresult::TIMEOUT,
            eresult::SERVICE_UNAVAILABLE,
            eresult::TRY_ANOTHER_CM,
            eresult::RATE_LIMIT_EXCEEDED,
        ] {
            assert!(
                matches!(logon_rejection(er, String::new()), Error::LogonRetryable(_)),
                "eresult {er} should be retryable"
            );
        }
    }

    #[test]
    fn try_another_cm_is_forty_eight() {
        // 42 is NoMatch. Getting this wrong silently disabled the transient
        // heal for the commonest load-balancing logoff Valve sends.
        assert_eq!(eresult::TRY_ANOTHER_CM, 48);
        assert!(logon_eresult_retryable(48));
        assert!(!logon_eresult_retryable(42));
    }

    #[test]
    fn terminal_logon_eresults_are_auth_rejected() {
        // InvalidPassword, AccountLogonDenied, AccountLoginDeniedNeedTwoFactor,
        // TwoFactorCodeMismatch, plus the proto2 default and an unknown code.
        for er in [5, 63, 85, 88, eresult::FAIL, 9999] {
            assert!(
                matches!(logon_rejection(er, String::new()), Error::AuthRejected(_)),
                "eresult {er} should be terminal"
            );
        }
    }
}
