//! Friends operations over a logged-in CM session — list, add, remove.
//!
//! All keyless, riding on a [`SessionHandle`]:
//!
//! - [`add_friend`] / [`add_friend_by_name`] — send a friend request (or accept
//!   an incoming one), via the job-correlated `CMsgClientAddFriend`.
//! - [`remove_friend`] — remove a friend or decline a request
//!   (`CMsgClientRemoveFriend`).
//! - [`request_friends_list`] — the account's friend list, captured from the
//!   `CMsgClientFriendsList` Steam pushes right after login.
//!
//! Pair these with [`crate::persona`] to turn friend `SteamID`s into names and
//! avatars.

use std::time::Duration;

use prost::Message;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::proto::{
    CMsgClientAddFriend, CMsgClientAddFriendResponse, CMsgClientFriendsList, CMsgClientRemoveFriend,
};
use crate::session::SessionHandle;
use crate::{Error, Result};

// EMsg values, from `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_REMOVE_FRIEND: u32 = 714;
const EMSG_CLIENT_FRIENDS_LIST: u32 = 767;
const EMSG_CLIENT_ADD_FRIEND: u32 = 791;

/// `EResult::OK`.
const ERESULT_OK: i32 = 1;

/// How long to wait for the post-login friends list.
const FRIENDS_LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// How we relate to another account (`EFriendRelationship`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FriendRelationship {
    /// No relationship.
    None,
    /// We blocked them.
    Blocked,
    /// They sent us a friend request (we are the recipient).
    RequestRecipient,
    /// Mutual friends.
    Friend,
    /// We sent them a friend request (we are the initiator).
    RequestInitiator,
    /// We ignored their request.
    Ignored,
    /// A former friend who is now ignored.
    IgnoredFriend,
    /// A value newer than this enum knows about.
    Unknown(u32),
}

impl FriendRelationship {
    fn from_raw(value: u32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Blocked,
            2 => Self::RequestRecipient,
            3 => Self::Friend,
            4 => Self::RequestInitiator,
            5 => Self::Ignored,
            6 => Self::IgnoredFriend,
            other => Self::Unknown(other),
        }
    }
}

/// One entry in the friends list.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Friend {
    /// The other account's 64-bit `SteamID`.
    pub steam_id: u64,
    /// How we relate to them.
    pub relationship: FriendRelationship,
}

/// The account whose friend request we just sent / accepted.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AddedFriend {
    /// The added account's 64-bit `SteamID`.
    pub steam_id: u64,
    /// Their persona name at the time, if Steam returned it.
    pub persona_name: String,
}

/// Send a friend request to (or accept one from) `steam_id`.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the request (already friends, blocked,
/// limit hit, …), [`Error::WebSocket`] if the session stopped, or a transport /
/// decode error.
pub async fn add_friend(session: &SessionHandle, steam_id: u64) -> Result<AddedFriend> {
    let request = CMsgClientAddFriend {
        steamid_to_add: Some(steam_id),
        ..Default::default()
    };
    add(session, &request).await
}

/// Send a friend request by account name or email.
///
/// # Errors
///
/// As [`add_friend`].
pub async fn add_friend_by_name(
    session: &SessionHandle,
    name_or_email: &str,
) -> Result<AddedFriend> {
    let request = CMsgClientAddFriend {
        accountname_or_email_to_add: Some(name_or_email.to_string()),
        ..Default::default()
    };
    add(session, &request).await
}

/// Shared `AddFriend` request/response handling.
async fn add(session: &SessionHandle, request: &CMsgClientAddFriend) -> Result<AddedFriend> {
    let response: CMsgClientAddFriendResponse =
        session.request(EMSG_CLIENT_ADD_FRIEND, request).await?;

    let eresult = response.eresult.unwrap_or(2);
    if eresult != ERESULT_OK {
        return Err(Error::Remote(format!("add friend: eresult {eresult}")));
    }
    Ok(AddedFriend {
        steam_id: response.steam_id_added.unwrap_or(0),
        persona_name: response.persona_name_added.unwrap_or_default(),
    })
}

/// Remove a friend, or decline / cancel a friend request, for `steam_id`.
///
/// Fire-and-forget: Steam doesn't reply, so success only means the message was
/// sent. The relationship change shows up in a later `CMsgClientFriendsList`.
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped.
pub async fn remove_friend(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientRemoveFriend {
        friendid: Some(steam_id),
    };
    session.notify(EMSG_CLIENT_REMOVE_FRIEND, &request).await
}

/// Capture the account's friends list.
///
/// Steam pushes the full `CMsgClientFriendsList` **once, shortly after login**,
/// so call this right after [`spawn_session`](crate::session::spawn_session) —
/// before awaiting other work — to catch it. Later calls only see incremental
/// relationship updates and otherwise time out.
///
/// # Errors
///
/// [`Error::Timeout`] if no list arrives in time, [`Error::WebSocket`] if the
/// session stopped, or a decode error.
pub async fn request_friends_list(session: &SessionHandle) -> Result<Vec<Friend>> {
    let mut events = session.subscribe();
    let wait = async {
        loop {
            match events.recv().await {
                Ok(msg) if msg.emsg == EMSG_CLIENT_FRIENDS_LIST => {
                    let list = CMsgClientFriendsList::decode(msg.body.as_slice())
                        .map_err(|e| Error::Codec(format!("decode friends list: {e}")))?;
                    // Skip incremental deltas; we want the full post-login list.
                    if list.bincremental == Some(true) {
                        continue;
                    }
                    return Ok(friends_from(list));
                }
                // Some other message or a lagged slot: keep waiting.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::WebSocket("session stopped".into()))
                }
            }
        }
    };
    timeout(FRIENDS_LIST_TIMEOUT, wait)
        .await
        .map_err(|_| Error::Timeout("friends list"))?
}

/// Map a decoded `CMsgClientFriendsList` into our [`Friend`] entries.
fn friends_from(list: CMsgClientFriendsList) -> Vec<Friend> {
    list.friends
        .into_iter()
        .filter_map(|f| {
            Some(Friend {
                steam_id: f.ulfriendid?,
                relationship: FriendRelationship::from_raw(f.efriendrelationship.unwrap_or(0)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::SteamMessage;
    use crate::proto::{c_msg_client_friends_list::Friend as ProtoFriend, CMsgProtoBufHeader};

    #[test]
    fn relationship_maps_known_values() {
        assert_eq!(FriendRelationship::from_raw(0), FriendRelationship::None);
        assert_eq!(FriendRelationship::from_raw(3), FriendRelationship::Friend);
        assert_eq!(
            FriendRelationship::from_raw(4),
            FriendRelationship::RequestInitiator
        );
        assert_eq!(
            FriendRelationship::from_raw(42),
            FriendRelationship::Unknown(42)
        );
    }

    #[test]
    fn friends_from_maps_entries_and_skips_idless() {
        let list = CMsgClientFriendsList {
            bincremental: Some(false),
            friends: vec![
                ProtoFriend {
                    ulfriendid: Some(111),
                    efriendrelationship: Some(3), // Friend
                },
                ProtoFriend {
                    ulfriendid: Some(222),
                    efriendrelationship: Some(2), // RequestRecipient
                },
                ProtoFriend {
                    ulfriendid: None, // dropped
                    efriendrelationship: Some(3),
                },
            ],
            ..Default::default()
        };
        let friends = friends_from(list);
        assert_eq!(friends.len(), 2);
        assert_eq!(friends[0].steam_id, 111);
        assert_eq!(friends[0].relationship, FriendRelationship::Friend);
        assert_eq!(
            friends[1].relationship,
            FriendRelationship::RequestRecipient
        );
    }

    #[test]
    fn friends_list_round_trips_through_a_frame() {
        // Encode like the wire, decode via the same path request_friends_list uses.
        let body = CMsgClientFriendsList {
            bincremental: Some(false),
            friends: vec![ProtoFriend {
                ulfriendid: Some(76_561_198_000_000_000),
                efriendrelationship: Some(3),
            }],
            ..Default::default()
        }
        .encode_to_vec();
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_FRIENDS_LIST,
            header: CMsgProtoBufHeader::default(),
            body,
        };

        let decoded = CMsgClientFriendsList::decode(msg.body.as_slice()).unwrap();
        let friends = friends_from(decoded);
        assert_eq!(friends.len(), 1);
        assert_eq!(friends[0].steam_id, 76_561_198_000_000_000);
    }
}
