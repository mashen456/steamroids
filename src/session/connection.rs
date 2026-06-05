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
use std::time::Duration;

use flate2::read::GzDecoder;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use prost::Message as _;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::codec::{self, SteamMessage};
use crate::proto::{
    CMsgClientHeartBeat, CMsgClientLogon, CMsgClientLogonResponse, CMsgMulti, CMsgProtoBufHeader,
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
/// `EResult::OK`.
const ERESULT_OK: i32 = 1;
/// Fallback heartbeat interval if Steam doesn't specify one.
const DEFAULT_HEARTBEAT_SECS: i32 = 9;

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

/// A live CM session over a WebSocket.
pub struct CmConnection {
    ws: SteamWebSocket,
    steam_id: u64,
    session_id: i32,
    /// Messages already pulled off the wire (e.g. unpacked from a `Multi`),
    /// waiting to be returned by [`Self::recv`].
    inbox: VecDeque<SteamMessage>,
}

impl CmConnection {
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
        read_message(&mut self.ws, &mut self.inbox).await
    }

    /// The session id Steam assigned at logon (0 before logon).
    pub fn session_id(&self) -> i32 {
        self.session_id
    }

    /// Consume the connection into the pieces a background driver needs: the
    /// WebSocket stream, the logged-on `SteamID` / session id, and any messages
    /// already buffered from a `Multi`.
    pub(crate) fn into_parts(self) -> (SteamWebSocket, u64, i32, VecDeque<SteamMessage>) {
        (self.ws, self.steam_id, self.session_id, self.inbox)
    }

    /// Log in over the CM using `refresh_token` (the token from the
    /// [`auth`](crate::auth) `WebAPI` flow). Drives `CMsgClientLogon` →
    /// `CMsgClientLogonResponse`.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRejected`] if Steam returns a non-OK `EResult` or logs us
    /// off; transport / codec errors propagate as-is.
    pub async fn logon(&mut self, account_name: &str, refresh_token: &str) -> Result<LoggedOn> {
        let logon = CMsgClientLogon {
            protocol_version: Some(PROTOCOL_VERSION),
            client_package_version: Some(CLIENT_PACKAGE_VERSION),
            client_language: Some("english".to_string()),
            client_os_type: Some(CLIENT_OS_WINDOWS_10),
            should_remember_password: Some(false),
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
                    let eresult = resp.eresult.unwrap_or(2);
                    if eresult != ERESULT_OK {
                        return Err(Error::AuthRejected(format!("CM logon eresult {eresult}")));
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
                    return Err(Error::AuthRejected("CM logged us off during logon".into()));
                }
                // Ignore anything else (server lists, etc.) until the response.
                _ => {}
            }
        }
    }

    /// Send a single `CMsgClientHeartBeat`. Steam drops the session if these
    /// stop arriving (interval from [`LoggedOn::heartbeat_interval`]).
    pub async fn send_heartbeat(&mut self) -> Result<()> {
        self.send(EMSG_CLIENT_HEARTBEAT, &CMsgClientHeartBeat::default())
            .await
    }

    /// Keep the session alive: send a heartbeat every `interval` and hand every
    /// received message to `on_message`. Runs until Steam logs us off (returns
    /// `Ok(())`) or the connection / transport fails (returns `Err`).
    ///
    /// `recv` is cancel-safe, so the heartbeat deadline interrupts a pending
    /// read without losing data. Spawn this on its own task if the caller needs
    /// to do other work concurrently.
    pub async fn run<F>(&mut self, interval: Duration, mut on_message: F) -> Result<()>
    where
        F: FnMut(&SteamMessage),
    {
        let mut next_heartbeat = Instant::now() + interval;
        loop {
            if Instant::now() >= next_heartbeat {
                self.send_heartbeat().await?;
                next_heartbeat = Instant::now() + interval;
            }
            let wait = next_heartbeat.saturating_duration_since(Instant::now());
            match timeout(wait, self.recv()).await {
                Ok(result) => {
                    let msg = result?;
                    if msg.emsg == EMSG_CLIENT_LOGGED_OFF {
                        return Ok(());
                    }
                    on_message(&msg);
                }
                // Deadline hit with no message — loop and send the heartbeat.
                Err(_elapsed) => {}
            }
        }
    }
}

/// Read the next binary WebSocket frame from `read`, skipping control frames.
/// Generic over the stream so both [`CmConnection`] (full socket) and the
/// background driver (a split read half) can use it.
pub(crate) async fn read_frame<S>(read: &mut S) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<WsMessage, WsError>> + Unpin,
{
    loop {
        let ws_msg = read
            .next()
            .await
            .ok_or_else(|| Error::WebSocket("connection closed".into()))??;
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
) -> Result<SteamMessage>
where
    S: Stream<Item = std::result::Result<WsMessage, WsError>> + Unpin,
{
    loop {
        if let Some(msg) = inbox.pop_front() {
            return Ok(msg);
        }
        let frame = read_frame(read).await?;
        match codec::try_decode(&frame)? {
            Some(msg) if msg.emsg == EMSG_MULTI => inbox.extend(decode_multi(&msg.body)?),
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

/// Split a `CMsgMulti` body into its embedded messages. Each is a u32-LE
/// length prefix followed by a full codec frame; nested `Multi`s are flattened.
fn decode_multi(body: &[u8]) -> Result<Vec<SteamMessage>> {
    let multi = CMsgMulti::decode(body).map_err(|e| Error::Codec(format!("decode Multi: {e}")))?;
    let raw = multi.message_body.unwrap_or_default();
    // A non-zero `size_unzipped` means `message_body` is gzip-compressed.
    let payload = match multi.size_unzipped.unwrap_or(0) {
        0 => raw,
        size => gunzip(&raw, size as usize)?,
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
            Some(sub) if sub.emsg == EMSG_MULTI => out.extend(decode_multi(&sub.body)?),
            Some(sub) => out.push(sub),
            // Skip legacy non-protobuf sub-messages.
            None => {}
        }
        cur = &cur[len..];
    }
    Ok(out)
}

/// Gzip-decompress a `Multi` body, pre-sizing the buffer to `expected_len`.
fn gunzip(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(expected_len);
    GzDecoder::new(data)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Codec(format!("gunzip Multi: {e}")))?;
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

    #[test]
    fn decode_multi_splits_embedded_messages() {
        let body = multi_body(&[framed(703, 1), framed(751, 2)]);
        let msgs = decode_multi(&body).unwrap();
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
        let msgs = decode_multi(&body).unwrap();
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

        let msgs = decode_multi(&body).unwrap();
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
        assert!(matches!(decode_multi(&body).unwrap_err(), Error::Codec(_)));
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
        assert!(matches!(decode_multi(&body).unwrap_err(), Error::Codec(_)));
    }
}
