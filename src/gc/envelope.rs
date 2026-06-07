//! Game Coordinator message envelope: framing GC messages into (and out of) the
//! [`CMsgGcClient`](crate::proto::CMsgGcClient) relay carried over the CM.

use prost::Message;

use crate::codec::{self, SteamMessage};
use crate::proto::gc::CMsgProtoBufHeader as GcHeader;
use crate::proto::CMsgGcClient;
use crate::{Error, Result};

// EMsg values for the client<->GC relay, from `enums_clientserver.proto`.
pub(crate) const EMSG_CLIENT_TO_GC: u32 = 5452;
pub(crate) const EMSG_CLIENT_FROM_GC: u32 = 5453;

/// A decoded Game Coordinator message.
///
/// Mirrors [`SteamMessage`](crate::codec::SteamMessage) one layer down: it adds
/// the originating [`appid`](Self::appid) and uses the GC's own routing header.
/// Decode [`body`](Self::body) to the concrete `proto::gc` type for
/// [`msgtype`](Self::msgtype).
#[derive(Debug, Clone)]
pub struct GcMessage {
    /// App id of the Game Coordinator this came from (e.g. `730` for CS2).
    pub appid: u32,
    /// GC message type, with the protobuf flag stripped (an `EGCBaseClientMsg`
    /// or app-specific value).
    pub msgtype: u32,
    /// GC routing header (job ids, source app, eresult, …).
    pub header: GcHeader,
    /// Raw protobuf body for this GC message.
    pub body: Vec<u8>,
}

impl GcMessage {
    /// The job id this message is a reply to, or `None` when unset.
    ///
    /// The GC header defaults both job-id fields to the `u64::MAX` sentinel;
    /// this normalizes that (and `0`) to `None`. Note that many app GCs — CS2
    /// included — don't populate job ids on game messages, so correlation is
    /// usually by response *type* rather than job id.
    #[must_use]
    pub fn jobid_target(&self) -> Option<u64> {
        self.header
            .job_id_target
            .filter(|&j| j != 0 && j != u64::MAX)
    }
}

/// Wrap a GC message into a [`CMsgGcClient`] ready to send as `k_EMsgClientToGC`.
///
/// Builds the inner GC frame (`msgtype | proto-bit`, header length, `header`,
/// `body`) — the same framing as [`codec`], with the GC header type — and nests
/// it in the relay envelope tagged with `appid`.
#[must_use]
pub fn wrap(appid: u32, msgtype: u32, header: &GcHeader, body: &[u8]) -> CMsgGcClient {
    let payload = codec::encode_raw(msgtype, header, body);
    CMsgGcClient {
        appid: Some(appid),
        // The relay's msgtype carries the proto flag too, mirroring the payload.
        msgtype: Some(msgtype | codec::PROTO_MASK),
        payload: Some(payload),
        ..Default::default()
    }
}

/// Decode a `k_EMsgClientFromGC` [`SteamMessage`] into a [`GcMessage`].
///
/// Returns `Ok(None)` when the embedded GC message is a legacy non-protobuf one
/// (callers that only speak protobuf can skip it), mirroring
/// [`codec::try_decode`].
///
/// # Errors
///
/// [`Error::Codec`] if the relay envelope or the GC frame is malformed.
pub fn unwrap(msg: &SteamMessage) -> Result<Option<GcMessage>> {
    let client = CMsgGcClient::decode(msg.body.as_slice())
        .map_err(|e| Error::Codec(format!("decode CMsgGCClient: {e}")))?;
    let appid = client.appid.unwrap_or(0);
    let payload = client.payload.unwrap_or_default();
    match codec::unframe(&payload)? {
        None => Ok(None),
        Some(parts) => {
            let header = GcHeader::decode(parts.header)
                .map_err(|e| Error::Codec(format!("decode GC header: {e}")))?;
            Ok(Some(GcMessage {
                appid,
                msgtype: parts.msg_type,
                header,
                body: parts.body.to_vec(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPID: u32 = 730;
    const SOME_GC_MSG: u32 = 9128;

    /// Build the `ClientFromGC` `SteamMessage` Steam would deliver for a GC
    /// message produced by [`wrap`], so the round trip can be checked end to end.
    fn client_from_gc(client: &CMsgGcClient) -> SteamMessage {
        SteamMessage {
            emsg: EMSG_CLIENT_FROM_GC,
            header: crate::proto::CMsgProtoBufHeader::default(),
            body: client.encode_to_vec(),
        }
    }

    #[test]
    fn wrap_then_unwrap_round_trips() {
        let header = GcHeader {
            job_id_target: Some(99),
            ..Default::default()
        };
        let body = vec![1u8, 2, 3, 4];

        let client = wrap(APPID, SOME_GC_MSG, &header, &body);
        // The relay msgtype keeps the proto flag set.
        assert_eq!(client.msgtype, Some(SOME_GC_MSG | codec::PROTO_MASK));
        assert_eq!(client.appid, Some(APPID));

        let gc = unwrap(&client_from_gc(&client)).unwrap().expect("protobuf");
        assert_eq!(gc.appid, APPID);
        assert_eq!(gc.msgtype, SOME_GC_MSG, "proto flag stripped on decode");
        assert_eq!(gc.body, body);
        assert_eq!(gc.jobid_target(), Some(99));
    }

    #[test]
    fn jobid_target_normalizes_sentinels() {
        for sentinel in [0u64, u64::MAX] {
            let header = GcHeader {
                job_id_target: Some(sentinel),
                ..Default::default()
            };
            let client = wrap(APPID, SOME_GC_MSG, &header, &[]);
            let gc = unwrap(&client_from_gc(&client)).unwrap().unwrap();
            assert_eq!(gc.jobid_target(), None);
        }
    }

    #[test]
    fn unwrap_skips_non_protobuf_payload() {
        // A legacy GC message: proto bit clear in the inner payload.
        let mut payload = Vec::new();
        payload.extend_from_slice(&123u32.to_le_bytes()); // no PROTO_MASK
        payload.extend_from_slice(&0u32.to_le_bytes());
        let client = CMsgGcClient {
            appid: Some(APPID),
            msgtype: Some(123),
            payload: Some(payload),
            ..Default::default()
        };
        assert!(unwrap(&client_from_gc(&client)).unwrap().is_none());
    }

    #[test]
    fn unwrap_rejects_garbage_envelope() {
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_FROM_GC,
            header: crate::proto::CMsgProtoBufHeader::default(),
            // A truncated frame inside a valid envelope.
            body: CMsgGcClient {
                appid: Some(APPID),
                msgtype: Some(1),
                payload: Some(vec![0xff, 0x00]), // shorter than the 8-byte prefix
                ..Default::default()
            }
            .encode_to_vec(),
        };
        assert!(matches!(unwrap(&msg).unwrap_err(), Error::Codec(_)));
    }
}
