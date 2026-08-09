//! Friends operations over a logged-in CM session.
//!
//! All keyless, riding on a [`SessionHandle`]:
//!
//! **Relationships**
//!
//! - [`add_friend`] — send a friend request (or accept an incoming one), via
//!   the job-correlated `CMsgClientAddFriend`. Requires a `SteamID` — account
//!   names aren't unique on Steam, so there's no `add_friend_by_name`: the
//!   account must be addressed by its `SteamID64`. Use
//!   [`crate::persona::resolve_vanity_url`] for a vanity → `SteamID` resolve
//!   if you only have a vanity URL.
//! - [`remove_friend`] — remove a friend or decline a request
//!   (`CMsgClientRemoveFriend`).
//! - [`request_friends_list`] — the account's friend list, captured from the
//!   `CMsgClientFriendsList` Steam pushes right after login.
//!
//! **Per-friend nicknames**
//!
//! - [`set_nickname`] / [`clear_nickname`] — name a friend locally, visible
//!   only to you. Job-correlated `CMsgClientSetPlayerNickname` → AM.
//! - [`request_nicknames`] — the post-login nickname list, from
//!   `CMsgClientPlayerNicknameList`.
//!
//! **Visibility**
//!
//! - [`hide_friend`] / [`unhide_friend`] — collapse a friend in the friends
//!   list, via `CMsgClientHideFriend`.
//!
//! **Chat**
//!
//! - [`send_message`] — a plain text chat message
//!   (`CMsgClientFriendMsg`, `EChatEntryType_ChatMsg`).
//! - [`send_typing`] — a "user is typing…" notification
//!   (`EChatEntryType_Typing`).
//!
//! **Groups (custom tags in the friends list)**
//!
//! - [`create_friends_group`] / [`delete_friends_group`] /
//!   [`rename_friends_group`] — `CMsgClient{Create,Delete,Manage}FriendsGroup`
//!   → AM.
//! - [`add_friends_to_group`] / [`remove_friends_from_group`] —
//!   `CMsgClient{Add,Remove}FriendToGroup` → AM.
//! - [`request_friends_groups_list`] — the post-login group list, from
//!   `CMsgClientFriendsGroupsList`.
//!
//! Pair these with [`crate::persona`] to turn friend `SteamID`s into names and
//! avatars.

use std::time::Duration;

use prost::Message;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::error::eresult;
use crate::proto::{
    c_msg_client_friends_groups_list::{
        FriendGroup as ProtoFriendGroup, FriendGroupsMembership as ProtoMembership,
    },
    CMsgClientAddFriend, CMsgClientAddFriendResponse, CMsgClientAddFriendToGroup,
    CMsgClientAddFriendToGroupResponse, CMsgClientCreateFriendsGroup,
    CMsgClientCreateFriendsGroupResponse, CMsgClientDeleteFriendsGroup,
    CMsgClientDeleteFriendsGroupResponse, CMsgClientFriendMsg, CMsgClientFriendsGroupsList,
    CMsgClientFriendsList, CMsgClientHideFriend, CMsgClientManageFriendsGroup,
    CMsgClientManageFriendsGroupResponse, CMsgClientPlayerNicknameList, CMsgClientRemoveFriend,
    CMsgClientRemoveFriendFromGroup, CMsgClientRemoveFriendFromGroupResponse,
    CMsgClientSetPlayerNickname, CMsgClientSetPlayerNicknameResponse,
};
use crate::session::SessionHandle;
use crate::{Error, Result};

// EMsg values, from `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_REMOVE_FRIEND: u32 = 714;
const EMSG_CLIENT_FRIENDS_LIST: u32 = 767;
const EMSG_CLIENT_ADD_FRIEND: u32 = 791;
const EMSG_CLIENT_FRIEND_MSG: u32 = 718;
const EMSG_CLIENT_HIDE_FRIEND: u32 = 5552;
const EMSG_CLIENT_FRIENDS_GROUPS_LIST: u32 = 5553;
const EMSG_CLIENT_PLAYER_NICKNAME_LIST: u32 = 5587;
// `AMClient*` EMsgs: the message types are `CMsgClient*` (client-built), but the
// AM server handles them. The `AM` prefix is on the EMsg side only.
const EMSG_AM_CLIENT_SET_PLAYER_NICKNAME: u32 = 5588;
const EMSG_AM_CLIENT_CREATE_FRIENDS_GROUP: u32 = 5560;
const EMSG_AM_CLIENT_DELETE_FRIENDS_GROUP: u32 = 5562;
const EMSG_AM_CLIENT_MANAGE_FRIENDS_GROUP: u32 = 5564;
const EMSG_AM_CLIENT_ADD_FRIEND_TO_GROUP: u32 = 5566;
const EMSG_AM_CLIENT_REMOVE_FRIEND_FROM_GROUP: u32 = 5568;

/// `EResult::OK` as `u32`: the AM nickname and friends-group responses type
/// their `eresult` field unsigned.
const ERESULT_OK_U32: u32 = eresult::OK as u32;
/// `EResult::Fail` as `u32`, the proto2 default of those same fields.
const ERESULT_FAIL_U32: u32 = eresult::FAIL as u32;

/// How long to wait for the post-login friends list.
const FRIENDS_LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the post-login nickname list.
const NICKNAMES_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the post-login groups list.
const GROUPS_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// The `EResult` Steam answered with: `1` (`OK`) for a request that was
    /// newly sent or accepted, `29` (`DuplicateRequest`) when the relationship
    /// (or our pending request for it) already existed.
    pub eresult: i32,
}

/// A friend you have given a personal nickname to. Nicknames are local: only
/// you see them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Nickname {
    /// The friend's 64-bit `SteamID`.
    pub steam_id: u64,
    /// The nickname. Empty means the entry is a "clear nickname" marker from
    /// [`clear_nickname`].
    pub nickname: String,
}

/// A custom group (a tag) in the friends list. Membership is the same set of
/// `SteamID`s you see in [`crate::friends::Friend`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FriendsGroup {
    /// Server-assigned group id; pass it back to the rename / add / remove
    /// functions.
    pub group_id: i32,
    /// Group name.
    pub name: String,
    /// The `SteamID`s currently in the group.
    pub members: Vec<u64>,
}

/// What a fresh group looks like right after [`create_friends_group`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NewFriendsGroup {
    /// Server-assigned group id.
    pub group_id: i32,
    /// The name you gave it.
    pub name: String,
}

/// What a chat message in a friend conversation is. Only [`ChatEntryType::Text`]
/// and [`ChatEntryType::Typing`] are useful from a client; the rest are server-
/// emitted room events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatEntryType {
    /// A regular text message ([`send_message`] uses this).
    Text,
    /// A "user is typing…" indicator ([`send_typing`] uses this).
    Typing,
    /// An "invite to game" entry. The `message` field is a serialized
    /// `CMsgClientInviteToGame` in newer clients; older clients used a plain
    /// string.
    InviteGame,
    /// A value newer than this enum knows about.
    Unknown(i32),
}

impl ChatEntryType {
    /// The `EChatEntryType` value this maps to on the wire.
    pub fn raw(self) -> i32 {
        match self {
            Self::Text => 1,
            Self::Typing => 2,
            Self::InviteGame => 3,
            Self::Unknown(other) => other,
        }
    }

    /// Map a wire `EChatEntryType` value, keeping unknown ones intact.
    pub fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::Text,
            2 => Self::Typing,
            3 => Self::InviteGame,
            other => Self::Unknown(other),
        }
    }
}

/// Send a friend request to (or accept an incoming one from) `steam_id`.
///
/// Steam answers with an `EResult`. `OK` (the request was sent, or an incoming
/// one accepted) and `DuplicateRequest` (the relationship already exists) both
/// return `Ok(AddedFriend)` carrying the resolved target `SteamID` and persona
/// name; read [`AddedFriend::eresult`] to tell "added" from "already a friend".
/// Every other `EResult` (blocked, rate-limited, user not found, …) surfaces
/// as [`Error::Remote`].
///
/// `steam_id` is the **only** supported target identifier — there is no
/// `add_friend_by_name`, because Steam account names are not unique and
/// `CMsgClientAddFriend` with `accountname_or_email_to_add` returns an
/// ambiguous match. If you only have a vanity URL, resolve it first with
/// [`crate::persona::resolve_vanity_url`].
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the request, [`Error::WebSocket`] if the
/// session stopped, or a transport / decode error.
pub async fn add_friend(session: &SessionHandle, steam_id: u64) -> Result<AddedFriend> {
    let request = CMsgClientAddFriend {
        steamid_to_add: Some(steam_id),
        ..Default::default()
    };
    add(session, &request).await
}

/// Shared `AddFriend` request/response handling.
///
/// The `EResult` decides, not the presence of `steam_id_added`: only `OK` and
/// `DuplicateRequest` are success. `DuplicateRequest` stays non-fatal because
/// Steam returns the existing friendship's `steam_id_added` and persona name
/// with it, which is what a caller resolving an id wants either way.
///
/// The body result is the only one that counts, so this takes
/// [`SessionHandle::request_ignoring_eresult`]: the generic header check would
/// otherwise reject a `DuplicateRequest` before we ever see the body, should
/// Steam mirror the code into the routing header.
async fn add(session: &SessionHandle, request: &CMsgClientAddFriend) -> Result<AddedFriend> {
    let response: CMsgClientAddFriendResponse = session
        .request_ignoring_eresult(EMSG_CLIENT_ADD_FRIEND, request)
        .await?;

    let code = response.eresult.unwrap_or(eresult::FAIL);
    if code != eresult::OK && code != eresult::DUPLICATE_REQUEST {
        return Err(Error::Remote(format!("add friend: eresult {code}")));
    }
    let steam_id = response
        .steam_id_added
        .filter(|&id| id != 0)
        .ok_or_else(|| Error::Remote(format!("add friend: eresult {code}, no steam id")))?;

    Ok(AddedFriend {
        steam_id,
        persona_name: response.persona_name_added.unwrap_or_default(),
        eresult: code,
    })
}

/// Remove a friend, or decline / cancel a friend request, for `steam_id`.
///
/// Fire-and-forget: Steam doesn't reply, so success only means the message was
/// sent. The relationship change shows up in a later `CMsgClientFriendsList`.
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped or the write failed.
pub async fn remove_friend(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientRemoveFriend {
        friendid: Some(steam_id),
    };
    session.notify(EMSG_CLIENT_REMOVE_FRIEND, &request).await
}

/// Capture the account's friends list.
///
/// Steam pushes the full `CMsgClientFriendsList` **once, shortly after login**.
/// The session caches that snapshot, so this returns it whenever you call it —
/// no need to race the push by calling right after
/// [`spawn_session`](crate::session::spawn_session). The cache is refreshed on
/// every reconnect.
///
/// # Errors
///
/// [`Error::Timeout`] if no list has arrived yet and none does in time,
/// [`Error::WebSocket`] if the session stopped, or a decode error.
pub async fn request_friends_list(session: &SessionHandle) -> Result<Vec<Friend>> {
    let mut events = session.subscribe();
    // Subscribe first, then read the cache: the session caches the post-login
    // snapshot, so if it already arrived we return it here instead of timing
    // out. Any snapshot not yet cached is still guaranteed to reach `events`.
    if let Some(body) = session.cached_snapshot(EMSG_CLIENT_FRIENDS_LIST) {
        let list = CMsgClientFriendsList::decode(body.as_slice())
            .map_err(|e| Error::Codec(format!("decode friends list: {e}")))?;
        if list.bincremental != Some(true) {
            return Ok(friends_from(list));
        }
    }
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

/// Set the personal nickname `nickname` for `steam_id`. Visible only to this
/// account.
///
/// `nickname` is a UTF-8 string up to roughly 32 bytes; Steam truncates or
/// rejects longer values — no client-side check, the server enforces it. Pass
/// an empty string to clear, or use [`clear_nickname`] for intent.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the request (e.g. that account isn't on
/// your friends list), [`Error::WebSocket`] if the session stopped, or a
/// transport / decode error.
pub async fn set_nickname(session: &SessionHandle, steam_id: u64, nickname: &str) -> Result<()> {
    let request = CMsgClientSetPlayerNickname {
        steamid: Some(steam_id),
        nickname: Some(nickname.to_string()),
    };
    let response: CMsgClientSetPlayerNicknameResponse = session
        .request(EMSG_AM_CLIENT_SET_PLAYER_NICKNAME, &request)
        .await?;
    let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
    if code != ERESULT_OK_U32 {
        return Err(Error::Remote(format!("set nickname: eresult {code}")));
    }
    Ok(())
}

/// Clear the personal nickname for `steam_id`.
///
/// Equivalent to [`set_nickname`] with an empty string, but reads as intent.
///
/// # Errors
///
/// As [`set_nickname`].
pub async fn clear_nickname(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientSetPlayerNickname {
        steamid: Some(steam_id),
        nickname: Some(String::new()),
    };
    let response: CMsgClientSetPlayerNicknameResponse = session
        .request(EMSG_AM_CLIENT_SET_PLAYER_NICKNAME, &request)
        .await?;
    let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
    if code != ERESULT_OK_U32 {
        return Err(Error::Remote(format!("clear nickname: eresult {code}")));
    }
    Ok(())
}

/// Capture the post-login nickname list.
///
/// Like [`request_friends_list`], Steam pushes `CMsgClientPlayerNicknameList`
/// once shortly after login and the session caches it, so this returns the
/// cached snapshot whenever you call it. Note Steam only pushes the list when
/// the account actually has nicknames set — for an account with none it never
/// arrives, so a [`Error::Timeout`] here is expected and benign. The list
/// includes entries for nicknames you previously cleared (the `nickname` field
/// is empty in that case).
///
/// # Errors
///
/// [`Error::Timeout`] if no list has arrived yet and none does in time,
/// [`Error::WebSocket`] if the session stopped, or a decode error.
pub async fn request_nicknames(session: &SessionHandle) -> Result<Vec<Nickname>> {
    let mut events = session.subscribe();
    // Subscribe first, then read the cache (see `request_friends_list`).
    if let Some(body) = session.cached_snapshot(EMSG_CLIENT_PLAYER_NICKNAME_LIST) {
        let list = CMsgClientPlayerNicknameList::decode(body.as_slice())
            .map_err(|e| Error::Codec(format!("decode nicknames: {e}")))?;
        if is_full_nicknames(&list) {
            return Ok(nicknames_from(list));
        }
    }
    let wait = async {
        loop {
            match events.recv().await {
                Ok(msg) if msg.emsg == EMSG_CLIENT_PLAYER_NICKNAME_LIST => {
                    let list = CMsgClientPlayerNicknameList::decode(msg.body.as_slice())
                        .map_err(|e| Error::Codec(format!("decode nicknames: {e}")))?;
                    // Deltas and removal pushes aren't the post-login dump.
                    if !is_full_nicknames(&list) {
                        continue;
                    }
                    return Ok(nicknames_from(list));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::WebSocket("session stopped".into()))
                }
            }
        }
    };
    timeout(NICKNAMES_TIMEOUT, wait)
        .await
        .map_err(|_| Error::Timeout("nicknames list"))?
}

/// Hide a friend in the friends list. They are not unfriended; you still see
/// them, just collapsed.
///
/// Fire-and-forget: Steam doesn't reply, so success only means the message
/// was sent. The effect shows up in a later `CMsgClientFriendsList`.
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped or the write failed.
pub async fn hide_friend(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientHideFriend {
        friendid: Some(steam_id),
        hide: Some(true),
    };
    session.notify(EMSG_CLIENT_HIDE_FRIEND, &request).await
}

/// Un-hide a previously hidden friend.
///
/// Fire-and-forget, like [`hide_friend`].
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped or the write failed.
pub async fn unhide_friend(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientHideFriend {
        friendid: Some(steam_id),
        hide: Some(false),
    };
    session.notify(EMSG_CLIENT_HIDE_FRIEND, &request).await
}

/// Send a plain text chat message to `steam_id`.
///
/// `text` is the UTF-8 message body. The Steam client truncates very long
/// messages; treat this as a chat line, not a file transfer. For "is typing"
/// notifications, use [`send_typing`].
///
/// Fire-and-forget: `CMsgClientFriendMsg` is not job-correlated and Steam sends
/// no reply, so `Ok` means the frame was written to the socket, **not** that
/// the message was accepted or delivered. A message Steam drops (not friends,
/// muted, rate-limited, a limited account) is indistinguishable from a
/// delivered one here.
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped or the write failed.
pub async fn send_message(session: &SessionHandle, steam_id: u64, text: &str) -> Result<()> {
    let request = CMsgClientFriendMsg {
        steamid: Some(steam_id),
        chat_entry_type: Some(ChatEntryType::Text.raw()),
        message: Some(text.as_bytes().to_vec()),
        ..Default::default()
    };
    session.notify(EMSG_CLIENT_FRIEND_MSG, &request).await
}

/// Send a "user is typing…" notification to `steam_id`.
///
/// Fire-and-forget, exactly like [`send_message`]: `Ok` means sent, not
/// delivered. The official client expires these after a few seconds.
///
/// # Errors
///
/// [`Error::WebSocket`] if the session stopped or the write failed.
pub async fn send_typing(session: &SessionHandle, steam_id: u64) -> Result<()> {
    let request = CMsgClientFriendMsg {
        steamid: Some(steam_id),
        chat_entry_type: Some(ChatEntryType::Typing.raw()),
        ..Default::default()
    };
    session.notify(EMSG_CLIENT_FRIEND_MSG, &request).await
}

/// Create a custom friends group, optionally with `members` in it.
///
/// `name` is UTF-8; Steam caps it server-side. Returns the server-assigned
/// `group_id` for use in subsequent rename / add / remove calls.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the request (e.g. too many groups) or
/// answers `OK` without a group id, [`Error::WebSocket`] if the session
/// stopped, or a transport / decode error.
pub async fn create_friends_group(
    session: &SessionHandle,
    name: &str,
    members: &[u64],
) -> Result<NewFriendsGroup> {
    let request = CMsgClientCreateFriendsGroup {
        steamid: None,
        groupname: Some(name.to_string()),
        steamid_friends: members.to_vec(),
    };
    let response: CMsgClientCreateFriendsGroupResponse = session
        .request(EMSG_AM_CLIENT_CREATE_FRIENDS_GROUP, &request)
        .await?;
    let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
    if code != ERESULT_OK_U32 {
        return Err(Error::Remote(format!(
            "create friends group: eresult {code}"
        )));
    }
    // no sentinel: 0 is a usable group id
    let group_id = response
        .groupid
        .ok_or_else(|| Error::Remote("create friends group: OK without a groupid".to_string()))?;
    Ok(NewFriendsGroup {
        group_id,
        name: name.to_string(),
    })
}

/// Delete a custom friends group. Members stay on the friends list; only the
/// tag is removed.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the request (unknown group, etc.),
/// [`Error::WebSocket`] if the session stopped, or a transport / decode error.
pub async fn delete_friends_group(session: &SessionHandle, group_id: i32) -> Result<()> {
    let request = CMsgClientDeleteFriendsGroup {
        steamid: None,
        groupid: Some(group_id),
    };
    let response: CMsgClientDeleteFriendsGroupResponse = session
        .request(EMSG_AM_CLIENT_DELETE_FRIENDS_GROUP, &request)
        .await?;
    let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
    if code != ERESULT_OK_U32 {
        return Err(Error::Remote(format!(
            "delete friends group: eresult {code}"
        )));
    }
    Ok(())
}

/// Rename a custom friends group.
///
/// # Errors
///
/// As [`create_friends_group`].
pub async fn rename_friends_group(
    session: &SessionHandle,
    group_id: i32,
    new_name: &str,
) -> Result<()> {
    let request = CMsgClientManageFriendsGroup {
        groupid: Some(group_id),
        groupname: Some(new_name.to_string()),
        steamid_friends_added: Vec::new(),
        steamid_friends_removed: Vec::new(),
    };
    let response: CMsgClientManageFriendsGroupResponse = session
        .request(EMSG_AM_CLIENT_MANAGE_FRIENDS_GROUP, &request)
        .await?;
    let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
    if code != ERESULT_OK_U32 {
        return Err(Error::Remote(format!(
            "rename friends group: eresult {code}"
        )));
    }
    Ok(())
}

/// Add one or more friends to an existing group.
///
/// The wire message carries a single `SteamID` per call, so this loops
/// internally, sending one job-correlated request per `SteamID`. The first
/// rejection short-circuits and returns [`Error::Remote`].
///
/// # Errors
///
/// As [`create_friends_group`].
pub async fn add_friends_to_group(
    session: &SessionHandle,
    group_id: i32,
    steam_ids: &[u64],
) -> Result<()> {
    for &sid in steam_ids {
        let request = CMsgClientAddFriendToGroup {
            groupid: Some(group_id),
            steamiduser: Some(sid),
        };
        let response: CMsgClientAddFriendToGroupResponse = session
            .request(EMSG_AM_CLIENT_ADD_FRIEND_TO_GROUP, &request)
            .await?;
        let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
        if code != ERESULT_OK_U32 {
            return Err(Error::Remote(format!(
                "add friend {sid} to group {group_id}: eresult {code}"
            )));
        }
    }
    Ok(())
}

/// Remove one or more friends from an existing group. The friends themselves
/// stay on the friends list; only the membership is removed.
///
/// Loops per `SteamID`, like [`add_friends_to_group`].
///
/// # Errors
///
/// As [`create_friends_group`].
pub async fn remove_friends_from_group(
    session: &SessionHandle,
    group_id: i32,
    steam_ids: &[u64],
) -> Result<()> {
    for &sid in steam_ids {
        let request = CMsgClientRemoveFriendFromGroup {
            groupid: Some(group_id),
            steamiduser: Some(sid),
        };
        let response: CMsgClientRemoveFriendFromGroupResponse = session
            .request(EMSG_AM_CLIENT_REMOVE_FRIEND_FROM_GROUP, &request)
            .await?;
        let code = response.eresult.unwrap_or(ERESULT_FAIL_U32);
        if code != ERESULT_OK_U32 {
            return Err(Error::Remote(format!(
                "remove friend {sid} from group {group_id}: eresult {code}"
            )));
        }
    }
    Ok(())
}

/// Capture the post-login friends-groups list.
///
/// Like [`request_friends_list`], Steam pushes `CMsgClientFriendsGroupsList`
/// once shortly after login and the session caches it, so this returns the
/// cached snapshot whenever you call it. Steam only pushes the list when the
/// account has groups defined, so a [`Error::Timeout`] for an account with none
/// is expected and benign.
///
/// # Errors
///
/// [`Error::Timeout`] if no list has arrived yet and none does in time,
/// [`Error::WebSocket`] if the session stopped, or a decode error.
pub async fn request_friends_groups_list(session: &SessionHandle) -> Result<Vec<FriendsGroup>> {
    let mut events = session.subscribe();
    // Subscribe first, then read the cache (see `request_friends_list`).
    if let Some(body) = session.cached_snapshot(EMSG_CLIENT_FRIENDS_GROUPS_LIST) {
        let list = CMsgClientFriendsGroupsList::decode(body.as_slice())
            .map_err(|e| Error::Codec(format!("decode groups list: {e}")))?;
        if is_full_groups(&list) {
            return Ok(groups_from(list));
        }
    }
    let wait = async {
        loop {
            match events.recv().await {
                Ok(msg) if msg.emsg == EMSG_CLIENT_FRIENDS_GROUPS_LIST => {
                    let list = CMsgClientFriendsGroupsList::decode(msg.body.as_slice())
                        .map_err(|e| Error::Codec(format!("decode groups list: {e}")))?;
                    // We want the full post-login dump, not a delta or removal.
                    if !is_full_groups(&list) {
                        continue;
                    }
                    return Ok(groups_from(list));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::WebSocket("session stopped".into()))
                }
            }
        }
    };
    timeout(GROUPS_TIMEOUT, wait)
        .await
        .map_err(|_| Error::Timeout("friends groups list"))?
}

/// Whether a `CMsgClientPlayerNicknameList` is the full post-login dump rather
/// than an incremental delta or a removal push.
fn is_full_nicknames(list: &CMsgClientPlayerNicknameList) -> bool {
    !list.incremental.unwrap_or(false) && !list.removal.unwrap_or(false)
}

/// Whether a `CMsgClientFriendsGroupsList` is the full post-login dump rather
/// than an incremental delta or a removal push.
fn is_full_groups(list: &CMsgClientFriendsGroupsList) -> bool {
    !list.bincremental.unwrap_or(false) && !list.bremoval.unwrap_or(false)
}

/// Map a decoded `CMsgClientPlayerNicknameList` into our [`Nickname`] entries.
fn nicknames_from(list: CMsgClientPlayerNicknameList) -> Vec<Nickname> {
    list.nicknames
        .into_iter()
        .filter_map(|n| {
            Some(Nickname {
                steam_id: n.steamid?,
                nickname: n.nickname.unwrap_or_default(),
            })
        })
        .collect()
}

/// Map a decoded `CMsgClientFriendsGroupsList` into our [`FriendsGroup`] entries.
fn groups_from(list: CMsgClientFriendsGroupsList) -> Vec<FriendsGroup> {
    let groups: Vec<ProtoFriendGroup> = list.friend_groups;
    let memberships: Vec<ProtoMembership> = list.memberships;
    groups
        .into_iter()
        .filter_map(|g| {
            let group_id = g.n_group_id?;
            let name = g.str_group_name.unwrap_or_default();
            let members = memberships
                .iter()
                .filter_map(|m| {
                    if m.n_group_id == Some(group_id) {
                        m.ul_steam_id
                    } else {
                        None
                    }
                })
                .collect();
            Some(FriendsGroup {
                group_id,
                name,
                members,
            })
        })
        .collect()
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
    use crate::session::driver::Command;

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

    // ---- New functionality -------------------------------------------------

    #[test]
    fn nicknames_from_maps_entries_and_skips_idless() {
        use crate::proto::c_msg_client_player_nickname_list::PlayerNickname;
        let list = CMsgClientPlayerNicknameList {
            removal: Some(false),
            incremental: Some(false),
            nicknames: vec![
                PlayerNickname {
                    steamid: Some(111),
                    nickname: Some("rival".into()),
                },
                PlayerNickname {
                    steamid: Some(222),
                    nickname: Some(String::new()), // cleared-nickname marker
                },
                PlayerNickname {
                    steamid: None, // dropped
                    nickname: Some("ghost".into()),
                },
            ],
        };
        let nicks = nicknames_from(list);
        assert_eq!(nicks.len(), 2);
        assert_eq!(nicks[0].steam_id, 111);
        assert_eq!(nicks[0].nickname, "rival");
        assert_eq!(nicks[1].steam_id, 222);
        assert_eq!(nicks[1].nickname, "");
    }

    #[test]
    fn nicknames_list_round_trips_through_a_frame() {
        use crate::proto::c_msg_client_player_nickname_list::PlayerNickname;
        let body = CMsgClientPlayerNicknameList {
            removal: Some(false),
            incremental: Some(false),
            nicknames: vec![PlayerNickname {
                steamid: Some(76_561_198_000_000_000),
                nickname: Some("old buddy".into()),
            }],
        }
        .encode_to_vec();
        let decoded = CMsgClientPlayerNicknameList::decode(body.as_slice()).unwrap();
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_PLAYER_NICKNAME_LIST,
            header: CMsgProtoBufHeader::default(),
            body: decoded.encode_to_vec(),
        };
        let nicks =
            nicknames_from(CMsgClientPlayerNicknameList::decode(msg.body.as_slice()).unwrap());
        assert_eq!(nicks.len(), 1);
        assert_eq!(nicks[0].nickname, "old buddy");
    }

    #[test]
    fn set_nickname_request_round_trips() {
        let req = CMsgClientSetPlayerNickname {
            steamid: Some(76_561_198_000_000_000),
            nickname: Some("bestie".into()),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientSetPlayerNickname::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.steamid, Some(76_561_198_000_000_000));
        assert_eq!(back.nickname.as_deref(), Some("bestie"));
    }

    #[test]
    fn set_nickname_response_eresult_ok_decodes() {
        let resp = CMsgClientSetPlayerNicknameResponse { eresult: Some(1) };
        let bytes = resp.encode_to_vec();
        let back = CMsgClientSetPlayerNicknameResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.eresult, Some(ERESULT_OK_U32));
    }

    #[test]
    fn hide_friend_request_round_trips() {
        let req = CMsgClientHideFriend {
            friendid: Some(76_561_198_000_000_000),
            hide: Some(true),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientHideFriend::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.friendid, Some(76_561_198_000_000_000));
        assert_eq!(back.hide, Some(true));
    }

    #[test]
    fn unhide_request_round_trips() {
        let req = CMsgClientHideFriend {
            friendid: Some(99),
            hide: Some(false),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientHideFriend::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.hide, Some(false));
    }

    #[test]
    fn send_message_request_round_trips() {
        let text = "hello, world";
        let req = CMsgClientFriendMsg {
            steamid: Some(76_561_198_000_000_000),
            chat_entry_type: Some(1), // EChatEntryType_ChatMsg
            message: Some(text.as_bytes().to_vec()),
            echo_to_sender: Some(false),
            rtime32_server_timestamp: None,
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientFriendMsg::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.steamid, Some(76_561_198_000_000_000));
        assert_eq!(back.chat_entry_type, Some(1));
        assert_eq!(back.message.as_deref(), Some(text.as_bytes()));
    }

    #[test]
    fn send_typing_request_round_trips() {
        let req = CMsgClientFriendMsg {
            steamid: Some(76_561_198_000_000_000),
            chat_entry_type: Some(2), // EChatEntryType_Typing
            message: None,
            echo_to_sender: None,
            rtime32_server_timestamp: None,
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientFriendMsg::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.chat_entry_type, Some(2));
        assert!(back.message.is_none());
    }

    #[test]
    fn create_friends_group_request_round_trips() {
        let req = CMsgClientCreateFriendsGroup {
            steamid: None,
            groupname: Some("cs2 duo partners".into()),
            steamid_friends: vec![11, 22, 33],
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientCreateFriendsGroup::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.groupname.as_deref(), Some("cs2 duo partners"));
        assert_eq!(back.steamid_friends, vec![11, 22, 33]);
    }

    #[test]
    fn create_friends_group_response_with_eresult_decodes() {
        let resp = CMsgClientCreateFriendsGroupResponse {
            eresult: Some(1),
            groupid: Some(7),
        };
        let bytes = resp.encode_to_vec();
        let back = CMsgClientCreateFriendsGroupResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.eresult, Some(ERESULT_OK_U32));
        assert_eq!(back.groupid, Some(7));
    }

    #[test]
    fn delete_friends_group_request_round_trips() {
        let req = CMsgClientDeleteFriendsGroup {
            steamid: None,
            groupid: Some(42),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientDeleteFriendsGroup::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.groupid, Some(42));
    }

    #[test]
    fn rename_via_manage_request_round_trips() {
        let req = CMsgClientManageFriendsGroup {
            groupid: Some(7),
            groupname: Some("new name".into()),
            steamid_friends_added: vec![],
            steamid_friends_removed: vec![],
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientManageFriendsGroup::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.groupid, Some(7));
        assert_eq!(back.groupname.as_deref(), Some("new name"));
    }

    #[test]
    fn add_friend_to_group_request_round_trips() {
        let req = CMsgClientAddFriendToGroup {
            groupid: Some(7),
            steamiduser: Some(76_561_198_000_000_000),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientAddFriendToGroup::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.groupid, Some(7));
        assert_eq!(back.steamiduser, Some(76_561_198_000_000_000));
    }

    #[test]
    fn remove_friend_from_group_request_round_trips() {
        let req = CMsgClientRemoveFriendFromGroup {
            groupid: Some(7),
            steamiduser: Some(76_561_198_000_000_000),
        };
        let bytes = req.encode_to_vec();
        let back = CMsgClientRemoveFriendFromGroup::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.groupid, Some(7));
        assert_eq!(back.steamiduser, Some(76_561_198_000_000_000));
    }

    #[test]
    fn groups_from_maps_entries_to_memberships() {
        use crate::proto::c_msg_client_friends_groups_list::{
            FriendGroup as ProtoFriendGroup, FriendGroupsMembership as ProtoMembership,
        };
        let list = CMsgClientFriendsGroupsList {
            bremoval: Some(false),
            bincremental: Some(false),
            friend_groups: vec![
                ProtoFriendGroup {
                    n_group_id: Some(1),
                    str_group_name: Some("cs2 duo partners".into()),
                },
                ProtoFriendGroup {
                    n_group_id: Some(2),
                    str_group_name: Some("work friends".into()),
                },
                ProtoFriendGroup {
                    n_group_id: None, // dropped
                    str_group_name: Some("ghost".into()),
                },
            ],
            memberships: vec![
                ProtoMembership {
                    ul_steam_id: Some(11),
                    n_group_id: Some(1),
                },
                ProtoMembership {
                    ul_steam_id: Some(22),
                    n_group_id: Some(1),
                },
                ProtoMembership {
                    ul_steam_id: Some(33),
                    n_group_id: Some(2),
                },
            ],
        };
        let groups = groups_from(list);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group_id, 1);
        assert_eq!(groups[0].name, "cs2 duo partners");
        assert_eq!(groups[0].members, vec![11, 22]);
        assert_eq!(groups[1].group_id, 2);
        assert_eq!(groups[1].members, vec![33]);
    }

    #[test]
    fn groups_list_round_trips_through_a_frame() {
        use crate::proto::c_msg_client_friends_groups_list::{
            FriendGroup as ProtoFriendGroup, FriendGroupsMembership as ProtoMembership,
        };
        let body = CMsgClientFriendsGroupsList {
            bremoval: Some(false),
            bincremental: Some(false),
            friend_groups: vec![ProtoFriendGroup {
                n_group_id: Some(99),
                str_group_name: Some("squad".into()),
            }],
            memberships: vec![ProtoMembership {
                ul_steam_id: Some(76_561_198_000_000_000),
                n_group_id: Some(99),
            }],
        }
        .encode_to_vec();
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_FRIENDS_GROUPS_LIST,
            header: CMsgProtoBufHeader::default(),
            body,
        };
        let decoded = CMsgClientFriendsGroupsList::decode(msg.body.as_slice()).unwrap();
        let groups = groups_from(decoded);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "squad");
        assert_eq!(groups[0].members, vec![76_561_198_000_000_000]);
    }

    #[test]
    fn emsg_constants_match_upstream_proto() {
        // Sanity check on the constants we hand-pinned, against the upstream
        // `enums_clientserver.proto` we vendored from. If a future bump
        // re-numbers any of these, this test will catch it.
        assert_eq!(EMSG_CLIENT_FRIEND_MSG, 718);
        assert_eq!(EMSG_CLIENT_HIDE_FRIEND, 5552);
        assert_eq!(EMSG_CLIENT_FRIENDS_GROUPS_LIST, 5553);
        assert_eq!(EMSG_CLIENT_PLAYER_NICKNAME_LIST, 5587);
        assert_eq!(EMSG_AM_CLIENT_SET_PLAYER_NICKNAME, 5588);
        assert_eq!(EMSG_AM_CLIENT_CREATE_FRIENDS_GROUP, 5560);
        assert_eq!(EMSG_AM_CLIENT_DELETE_FRIENDS_GROUP, 5562);
        assert_eq!(EMSG_AM_CLIENT_MANAGE_FRIENDS_GROUP, 5564);
        assert_eq!(EMSG_AM_CLIENT_ADD_FRIEND_TO_GROUP, 5566);
        assert_eq!(EMSG_AM_CLIENT_REMOVE_FRIEND_FROM_GROUP, 5568);
    }

    // ---- Response handling, driven through the test session seam -----------

    /// Run `op` against a driverless handle, answering its single request with
    /// `body`.
    async fn answer_one<T, F, Fut>(body: Vec<u8>, expect_emsg: u32, op: F) -> Result<T>
    where
        F: FnOnce(SessionHandle) -> Fut,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        answer_one_with_header(body, CMsgProtoBufHeader::default(), expect_emsg, op).await
    }

    /// [`answer_one`] with a caller-built routing header, to drive the
    /// header-level `EResult` check.
    async fn answer_one_with_header<T, F, Fut>(
        body: Vec<u8>,
        header: CMsgProtoBufHeader,
        expect_emsg: u32,
        op: F,
    ) -> Result<T>
    where
        F: FnOnce(SessionHandle) -> Fut,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(7);
        let task = tokio::spawn(op(handle));
        match commands.recv().await.expect("request queued") {
            Command::Request { emsg, reply, .. } => {
                assert_eq!(emsg, expect_emsg);
                reply
                    .send(Ok(SteamMessage {
                        emsg: expect_emsg,
                        header,
                        body,
                    }))
                    .expect("reply delivered");
            }
            _ => panic!("expected a Request"),
        }
        task.await.expect("op task")
    }

    async fn add_friend_answered(response: &CMsgClientAddFriendResponse) -> Result<AddedFriend> {
        answer_one(
            response.encode_to_vec(),
            EMSG_CLIENT_ADD_FRIEND,
            |h| async move { add_friend(&h, 76_561_198_000_000_001).await },
        )
        .await
    }

    #[tokio::test]
    async fn add_friend_accepts_ok() {
        let added = add_friend_answered(&CMsgClientAddFriendResponse {
            eresult: Some(eresult::OK),
            steam_id_added: Some(111),
            persona_name_added: Some("mate".into()),
        })
        .await
        .expect("ok is a success");
        assert_eq!(added.steam_id, 111);
        assert_eq!(added.persona_name, "mate");
        assert_eq!(added.eresult, eresult::OK);
    }

    #[tokio::test]
    async fn add_friend_accepts_duplicate_request() {
        let added = add_friend_answered(&CMsgClientAddFriendResponse {
            eresult: Some(eresult::DUPLICATE_REQUEST),
            steam_id_added: Some(222),
            persona_name_added: None,
        })
        .await
        .expect("already-friends stays non-fatal");
        assert_eq!(added.steam_id, 222);
        assert_eq!(added.eresult, eresult::DUPLICATE_REQUEST);
    }

    #[tokio::test]
    async fn add_friend_ignores_a_non_ok_header_eresult() {
        // DuplicateRequest mirrored into the routing header: the generic header
        // check would reject it, so add_friend must not be running it.
        let response = CMsgClientAddFriendResponse {
            eresult: Some(eresult::DUPLICATE_REQUEST),
            steam_id_added: Some(555),
            persona_name_added: Some("old mate".into()),
        };
        let header = CMsgProtoBufHeader {
            eresult: Some(eresult::DUPLICATE_REQUEST),
            ..Default::default()
        };
        let added = answer_one_with_header(
            response.encode_to_vec(),
            header,
            EMSG_CLIENT_ADD_FRIEND,
            |h| async move { add_friend(&h, 76_561_198_000_000_001).await },
        )
        .await
        .expect("the body result is the one that counts");
        assert_eq!(added.steam_id, 555);
        assert_eq!(added.eresult, eresult::DUPLICATE_REQUEST);
    }

    #[tokio::test]
    async fn add_friend_rejects_non_ok_eresult_carrying_a_steam_id() {
        // Steam identifies the target on a rejection too, so `steam_id_added`
        // alone can't mean success.
        let err = add_friend_answered(&CMsgClientAddFriendResponse {
            eresult: Some(25), // LimitExceeded
            steam_id_added: Some(333),
            persona_name_added: Some("stranger".into()),
        })
        .await
        .expect_err("a rejection must not read as success");
        match err {
            Error::Remote(text) => assert!(text.contains("eresult 25"), "{text}"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_friend_rejects_ok_without_a_steam_id() {
        let err = add_friend_answered(&CMsgClientAddFriendResponse {
            eresult: Some(eresult::OK),
            steam_id_added: None,
            persona_name_added: None,
        })
        .await
        .expect_err("no target id is not a usable success");
        assert!(matches!(err, Error::Remote(_)), "{err:?}");
    }

    #[tokio::test]
    async fn add_friend_defaults_a_missing_eresult_to_fail() {
        // proto2 default for this field is 2 (Fail), not 0.
        let err = add_friend_answered(&CMsgClientAddFriendResponse {
            eresult: None,
            steam_id_added: Some(444),
            persona_name_added: None,
        })
        .await
        .expect_err("missing eresult is Fail");
        match err {
            Error::Remote(text) => assert!(text.contains("eresult 2"), "{text}"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    async fn create_group_answered(
        response: &CMsgClientCreateFriendsGroupResponse,
    ) -> Result<NewFriendsGroup> {
        answer_one(
            response.encode_to_vec(),
            EMSG_AM_CLIENT_CREATE_FRIENDS_GROUP,
            |h| async move { create_friends_group(&h, "squad", &[]).await },
        )
        .await
    }

    #[tokio::test]
    async fn create_friends_group_keeps_a_zero_group_id() {
        let group = create_group_answered(&CMsgClientCreateFriendsGroupResponse {
            eresult: Some(1),
            groupid: Some(0),
        })
        .await
        .expect("0 is a valid group id");
        assert_eq!(group.group_id, 0);
        assert_eq!(group.name, "squad");
    }

    #[tokio::test]
    async fn create_friends_group_rejects_a_missing_group_id() {
        let err = create_group_answered(&CMsgClientCreateFriendsGroupResponse {
            eresult: Some(1),
            groupid: None,
        })
        .await
        .expect_err("no group id is not a success");
        assert!(matches!(err, Error::Remote(_)), "{err:?}");
    }

    #[tokio::test]
    async fn send_message_tags_the_frame_from_chat_entry_type() {
        let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(7);
        let task = tokio::spawn(async move { send_message(&handle, 111, "gg").await });
        match commands.recv().await.expect("notify queued") {
            Command::Notify { emsg, body, ack } => {
                assert_eq!(emsg, EMSG_CLIENT_FRIEND_MSG);
                let sent = CMsgClientFriendMsg::decode(body.as_slice()).expect("decodes");
                assert_eq!(sent.chat_entry_type, Some(ChatEntryType::Text.raw()));
                assert_eq!(sent.message.as_deref(), Some(b"gg".as_slice()));
                ack.send(Ok(())).expect("ack delivered");
            }
            _ => panic!("expected a Notify"),
        }
        task.await.expect("send task").expect("sent");
    }

    #[tokio::test]
    async fn send_typing_tags_the_frame_from_chat_entry_type() {
        let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(7);
        let task = tokio::spawn(async move { send_typing(&handle, 111).await });
        match commands.recv().await.expect("notify queued") {
            Command::Notify { body, ack, .. } => {
                let sent = CMsgClientFriendMsg::decode(body.as_slice()).expect("decodes");
                assert_eq!(sent.chat_entry_type, Some(ChatEntryType::Typing.raw()));
                assert!(sent.message.is_none());
                ack.send(Ok(())).expect("ack delivered");
            }
            _ => panic!("expected a Notify"),
        }
        task.await.expect("typing task").expect("sent");
    }

    #[test]
    fn chat_entry_type_round_trips_wire_values() {
        assert_eq!(ChatEntryType::Text.raw(), 1);
        assert_eq!(ChatEntryType::Typing.raw(), 2);
        assert_eq!(ChatEntryType::InviteGame.raw(), 3);
        for entry in [
            ChatEntryType::Text,
            ChatEntryType::Typing,
            ChatEntryType::InviteGame,
            ChatEntryType::Unknown(11),
        ] {
            assert_eq!(ChatEntryType::from_raw(entry.raw()), entry);
        }
    }

    fn nickname_list(
        removal: Option<bool>,
        incremental: Option<bool>,
    ) -> CMsgClientPlayerNicknameList {
        CMsgClientPlayerNicknameList {
            removal,
            incremental,
            nicknames: Vec::new(),
        }
    }

    #[test]
    fn full_nickname_list_excludes_deltas_and_removals() {
        assert!(is_full_nicknames(&nickname_list(Some(false), Some(false))));
        assert!(is_full_nicknames(&nickname_list(None, None)));
        assert!(
            !is_full_nicknames(&nickname_list(Some(true), Some(false))),
            "a removal push is not the post-login list"
        );
        assert!(!is_full_nicknames(&nickname_list(Some(false), Some(true))));
    }

    fn groups_list(
        bremoval: Option<bool>,
        bincremental: Option<bool>,
    ) -> CMsgClientFriendsGroupsList {
        CMsgClientFriendsGroupsList {
            bremoval,
            bincremental,
            friend_groups: Vec::new(),
            memberships: Vec::new(),
        }
    }

    #[test]
    fn full_groups_list_excludes_deltas_and_removals() {
        assert!(is_full_groups(&groups_list(Some(false), Some(false))));
        assert!(is_full_groups(&groups_list(None, None)));
        assert!(
            !is_full_groups(&groups_list(Some(true), Some(false))),
            "a removal push is not the post-login list"
        );
        assert!(!is_full_groups(&groups_list(Some(false), Some(true))));
    }
}
