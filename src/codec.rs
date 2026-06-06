//! Steam message framing for the Connection Manager WebSocket transport.
//!
//! Each binary WebSocket frame carries exactly one Steam message. Modern Steam
//! messages are protobuf-encoded and laid out little-endian as:
//!
//! ```text
//! ┌─────────────────┬───────────────┬────────────────────┬───────────┐
//! │ msg:  u32 LE    │ hdr_len: u32  │ CMsgProtoBufHeader  │ body      │
//! │ (EMsg | proto)  │ LE            │ (protobuf)          │ (protobuf)│
//! └─────────────────┴───────────────┴────────────────────┴───────────┘
//! ```
//!
//! The top bit of `msg` ([`PROTO_MASK`]) flags protobuf encoding; the remaining
//! bits are the [`EMsg`](crate::proto::EMsg) value. Over WSS there is **no**
//! `VT01` packet magic and **no** channel-encryption handshake — TLS already
//! secures the link, so the framing reduces to this envelope.

use prost::Message;

use crate::proto::CMsgProtoBufHeader;
use crate::{Error, Result};

/// Top bit of the message-type word: set when the payload is protobuf-encoded.
/// Every modern Steam message sets it.
pub const PROTO_MASK: u32 = 0x8000_0000;

/// Mask for the [`EMsg`](crate::proto::EMsg) value — every bit except the
/// protobuf flag.
pub const EMSG_MASK: u32 = !PROTO_MASK;

/// Minimum frame size: the two u32 length/type words.
const PREFIX_LEN: usize = 8;

/// A decoded Steam protobuf message: its `EMsg` type, routing header, and the
/// still-encoded body. Decode [`Self::body`] to the concrete `proto` type that
/// corresponds to [`Self::emsg`].
#[derive(Debug, Clone)]
pub struct SteamMessage {
    /// The `EMsg` type, with the protobuf flag stripped.
    pub emsg: u32,
    /// Routing / session header (`steamid`, `client_sessionid`, job ids, …).
    pub header: CMsgProtoBufHeader,
    /// Raw protobuf body for this `EMsg`.
    pub body: Vec<u8>,
}

/// Encode a protobuf Steam message into a single CM frame.
///
/// `emsg` is the bare [`EMsg`](crate::proto::EMsg) value; the protobuf flag is
/// applied here. Any extra high bits in `emsg` are masked off.
pub fn encode<M: Message>(emsg: u32, header: &CMsgProtoBufHeader, body: &M) -> Vec<u8> {
    encode_raw(emsg, header, &body.encode_to_vec())
}

/// Like [`encode`], but takes an already-serialized protobuf `body`. Useful when
/// the body bytes were produced elsewhere (e.g. queued for a background driver).
pub fn encode_raw(emsg: u32, header: &CMsgProtoBufHeader, body: &[u8]) -> Vec<u8> {
    frame(emsg, &header.encode_to_vec(), body)
}

/// Build a frame from an already-serialized header and body — the framing core
/// shared by the CM codec and the [`gc`](crate::gc) envelope, which only differ
/// in their header type. The protobuf flag is applied to `msg_type`; any stray
/// high bits are masked off.
pub(crate) fn frame(msg_type: u32, header_bytes: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX_LEN + header_bytes.len() + body.len());
    out.extend_from_slice(&((msg_type & EMSG_MASK) | PROTO_MASK).to_le_bytes());
    // A real header is far smaller than u32::MAX; the cast cannot wrap.
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(body);
    out
}

/// The borrowed parts of a frame after splitting off the prefix: the bare
/// message type and the still-encoded header and body slices. Returned by
/// [`unframe`].
pub(crate) struct FrameParts<'a> {
    /// Message type with the protobuf flag stripped.
    pub msg_type: u32,
    /// Still-encoded protobuf header bytes.
    pub header: &'a [u8],
    /// Still-encoded protobuf body bytes.
    pub body: &'a [u8],
}

/// Split a frame into its [`FrameParts`] with the protobuf flag stripped from
/// the message type. Returns `Ok(None)` for a legacy non-protobuf frame. The
/// byte-level counterpart to [`frame`].
///
/// # Errors
///
/// [`Error::Codec`] if the frame is shorter than the prefix or declares a header
/// longer than the frame.
pub(crate) fn unframe(bytes: &[u8]) -> Result<Option<FrameParts<'_>>> {
    if bytes.len() < PREFIX_LEN {
        return Err(Error::Codec("frame shorter than 8-byte prefix".into()));
    }
    let raw_msg = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    if raw_msg & PROTO_MASK == 0 {
        return Ok(None);
    }
    let header_len = u32::from_le_bytes(bytes[4..PREFIX_LEN].try_into().unwrap()) as usize;
    let rest = &bytes[PREFIX_LEN..];
    if rest.len() < header_len {
        return Err(Error::Codec(format!(
            "header length {header_len} exceeds {} remaining bytes",
            rest.len()
        )));
    }
    Ok(Some(FrameParts {
        msg_type: raw_msg & EMSG_MASK,
        header: &rest[..header_len],
        body: &rest[header_len..],
    }))
}

/// Like [`decode`], but returns `Ok(None)` for a legacy non-protobuf (packed
/// struct) message instead of an error. Steam still sends a few of these
/// (e.g. `ClientUpdateGuestPassesList`); callers that only speak protobuf can
/// skip them.
///
/// # Errors
///
/// As [`decode`], except a non-protobuf message is `Ok(None)` rather than an
/// error.
pub fn try_decode(bytes: &[u8]) -> Result<Option<SteamMessage>> {
    match unframe(bytes)? {
        None => Ok(None),
        Some(parts) => {
            let header = CMsgProtoBufHeader::decode(parts.header)
                .map_err(|e| Error::Codec(format!("decode header: {e}")))?;
            Ok(Some(SteamMessage {
                emsg: parts.msg_type,
                header,
                body: parts.body.to_vec(),
            }))
        }
    }
}

/// Decode one CM frame into its `EMsg`, header, and raw body.
///
/// # Errors
///
/// Returns [`Error::Codec`] if the frame is shorter than the prefix, declares a
/// header longer than the frame, is not protobuf-encoded, or the header fails
/// to decode.
pub fn decode(bytes: &[u8]) -> Result<SteamMessage> {
    try_decode(bytes)?.ok_or_else(|| {
        // `try_decode` only returns `None` once it has confirmed the prefix, so
        // the first word is present to report.
        let raw_msg = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        Error::Codec(format!(
            "non-protobuf message (raw type {raw_msg:#010x}) not supported"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // k_EMsgClientLogon, from protos/steam/enums_clientserver.proto.
    const EMSG_CLIENT_LOGON: u32 = 5514;

    #[test]
    fn round_trips_emsg_header_and_body() {
        let header = CMsgProtoBufHeader {
            steamid: Some(76_561_198_000_000_000),
            client_sessionid: Some(42),
            ..Default::default()
        };
        // Any protobuf Message is a valid body; reuse the header type so the
        // test is self-contained and proves a non-empty body survives.
        let body = CMsgProtoBufHeader {
            jobid_source: Some(7),
            ..Default::default()
        };

        let frame = encode(EMSG_CLIENT_LOGON, &header, &body);
        let msg = decode(&frame).unwrap();

        assert_eq!(msg.emsg, EMSG_CLIENT_LOGON);
        assert_eq!(msg.header.steamid, Some(76_561_198_000_000_000));
        assert_eq!(msg.header.client_sessionid, Some(42));

        let body_back = CMsgProtoBufHeader::decode(msg.body.as_slice()).unwrap();
        assert_eq!(body_back.jobid_source, Some(7));
    }

    #[test]
    fn encode_sets_proto_bit_and_masks_emsg() {
        let frame = encode(
            EMSG_CLIENT_LOGON,
            &CMsgProtoBufHeader::default(),
            &CMsgProtoBufHeader::default(),
        );
        let raw = u32::from_le_bytes(frame[..4].try_into().unwrap());
        assert_ne!(raw & PROTO_MASK, 0, "proto flag must be set");
        assert_eq!(raw & EMSG_MASK, EMSG_CLIENT_LOGON);
    }

    #[test]
    fn encode_strips_stray_high_bits_from_emsg() {
        // Passing an EMsg that already has the proto bit set must not corrupt it.
        let frame = encode(
            EMSG_CLIENT_LOGON | PROTO_MASK,
            &CMsgProtoBufHeader::default(),
            &CMsgProtoBufHeader::default(),
        );
        let raw = u32::from_le_bytes(frame[..4].try_into().unwrap());
        assert_eq!(raw & EMSG_MASK, EMSG_CLIENT_LOGON);
    }

    #[test]
    fn try_decode_skips_non_protobuf() {
        // Proto bit clear → a legacy message; try_decode returns None.
        let mut frame = Vec::new();
        frame.extend_from_slice(&798u32.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        assert!(try_decode(&frame).unwrap().is_none());
    }

    #[test]
    fn try_decode_returns_protobuf_message() {
        let frame = encode(
            EMSG_CLIENT_LOGON,
            &CMsgProtoBufHeader::default(),
            &CMsgProtoBufHeader::default(),
        );
        let msg = try_decode(&frame).unwrap().expect("protobuf message");
        assert_eq!(msg.emsg, EMSG_CLIENT_LOGON);
    }

    #[test]
    fn decode_rejects_short_frame() {
        let err = decode(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, Error::Codec(_)));
    }

    #[test]
    fn decode_rejects_non_protobuf_frame() {
        // Proto bit clear → unsupported.
        let mut frame = Vec::new();
        frame.extend_from_slice(&EMSG_CLIENT_LOGON.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        let err = decode(&frame).unwrap_err();
        assert!(matches!(err, Error::Codec(_)));
    }

    #[test]
    fn decode_rejects_oversized_header_length() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(EMSG_CLIENT_LOGON | PROTO_MASK).to_le_bytes());
        frame.extend_from_slice(&9999u32.to_le_bytes()); // claims a huge header
        frame.extend_from_slice(&[0u8; 4]);
        let err = decode(&frame).unwrap_err();
        assert!(matches!(err, Error::Codec(_)));
    }
}
