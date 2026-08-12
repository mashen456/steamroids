//! Offline coverage for the `friends` / `persona` / `wallet` / `licenses`
//! handlers.
//!
//! Each test drives the real public function against a driverless
//! `SessionHandle::for_test`, answering its command with a synthesized
//! `CMsgClient*` reply. That covers the parts a live test can't reach on
//! demand: rejections, partial pushes, and the snapshot cache (first-wins for
//! the post-login snapshots, friends and licenses; last-wins for wallet).
//!
//! Needs the unsupported `test-seam` feature (CI runs `--all-features`):
//!
//! ```text
//! cargo test --features test-seam --test session_handlers
//! ```
#![cfg(feature = "test-seam")]

use std::future::Future;
use std::time::Duration;

use prost::Message as _;
use tokio::sync::broadcast;

use steamroids::codec::SteamMessage;
use steamroids::friends;
use steamroids::licenses;
use steamroids::persona;
use steamroids::proto::{
    c_msg_client_friends_list::Friend as ProtoFriend,
    c_msg_client_license_list::License as ProtoLicense,
    c_msg_client_persona_state::Friend as Persona, CMsgClientAddFriendToGroupResponse,
    CMsgClientDeleteFriendsGroupResponse, CMsgClientFriendProfileInfoResponse,
    CMsgClientFriendsList, CMsgClientLicenseList, CMsgClientManageFriendsGroupResponse,
    CMsgClientPersonaState, CMsgClientRemoveFriendFromGroupResponse, CMsgClientRequestFriendData,
    CMsgClientSetPlayerNicknameResponse, CMsgClientWalletInfoUpdate, CMsgProtoBufHeader,
};
use steamroids::session::driver::Command;
use steamroids::session::SessionHandle;
use steamroids::wallet;
use steamroids::{Error, Result};

const SELF_ID: u64 = 76_561_198_000_000_000;
const OTHER_ID: u64 = 76_561_198_000_000_001;

// EMsg values, from `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_FRIENDS_LIST: u32 = 767;
const EMSG_CLIENT_PERSONA_STATE: u32 = 766;
const EMSG_CLIENT_REQUEST_FRIEND_DATA: u32 = 815;
const EMSG_CLIENT_FRIEND_PROFILE_INFO: u32 = 5535;
const EMSG_AM_CLIENT_SET_PLAYER_NICKNAME: u32 = 5588;
const EMSG_AM_CLIENT_MANAGE_FRIENDS_GROUP: u32 = 5564;
const EMSG_AM_CLIENT_DELETE_FRIENDS_GROUP: u32 = 5562;
const EMSG_AM_CLIENT_ADD_FRIEND_TO_GROUP: u32 = 5566;
const EMSG_AM_CLIENT_REMOVE_FRIEND_FROM_GROUP: u32 = 5568;
const EMSG_CLIENT_WALLET_INFO_UPDATE: u32 = 5528;
const EMSG_CLIENT_LICENSE_LIST: u32 = 780;

/// `EResult::LimitExceeded`, a plausible Steam-side rejection.
const ERESULT_LIMIT_EXCEEDED: u32 = 25;

/// Run `op` against a driverless handle, answering its single request with
/// `body` and the given header `eresult`.
async fn answer_request<T, F, Fut>(
    expect_emsg: u32,
    body: Vec<u8>,
    header_eresult: Option<i32>,
    op: F,
) -> Result<T>
where
    F: FnOnce(SessionHandle) -> Fut,
    Fut: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
    let task = tokio::spawn(op(handle));
    match commands.recv().await.expect("request queued") {
        Command::Request { emsg, reply, .. } => {
            assert_eq!(emsg, expect_emsg);
            reply
                .send(Ok(SteamMessage {
                    emsg: expect_emsg,
                    header: CMsgProtoBufHeader {
                        eresult: header_eresult,
                        ..Default::default()
                    },
                    body,
                }))
                .expect("reply delivered");
        }
        _ => panic!("expected a Request"),
    }
    task.await.expect("op task")
}

/// Yield until the op under test has subscribed, so a push can't be sent into
/// an empty channel and vanish. Bounded so a regression fails instead of hanging.
async fn await_subscriber(events: &broadcast::Sender<SteamMessage>) {
    for _ in 0..1_000 {
        if events.receiver_count() > 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the op never subscribed");
}

fn assert_remote(err: &Error, needle: &str) {
    match err {
        Error::Remote(text) => assert!(text.contains(needle), "{text}"),
        other => panic!("expected Remote, got {other:?}"),
    }
}

// ---- friends: rejections must not read as success -------------------------

#[tokio::test]
async fn set_nickname_accepts_ok() {
    answer_request(
        EMSG_AM_CLIENT_SET_PLAYER_NICKNAME,
        CMsgClientSetPlayerNicknameResponse { eresult: Some(1) }.encode_to_vec(),
        None,
        |h| async move { friends::set_nickname(&h, OTHER_ID, "rival").await },
    )
    .await
    .expect("eresult OK is a success");
}

#[tokio::test]
async fn set_nickname_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_AM_CLIENT_SET_PLAYER_NICKNAME,
        CMsgClientSetPlayerNicknameResponse {
            eresult: Some(ERESULT_LIMIT_EXCEEDED),
        }
        .encode_to_vec(),
        None,
        |h| async move { friends::set_nickname(&h, OTHER_ID, "rival").await },
    )
    .await
    .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
}

#[tokio::test]
async fn clear_nickname_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_AM_CLIENT_SET_PLAYER_NICKNAME,
        CMsgClientSetPlayerNicknameResponse {
            eresult: Some(ERESULT_LIMIT_EXCEEDED),
        }
        .encode_to_vec(),
        None,
        |h| async move { friends::clear_nickname(&h, OTHER_ID).await },
    )
    .await
    .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
}

#[tokio::test]
async fn an_empty_reply_body_is_the_proto2_fail_default() {
    // An empty body decodes cleanly into every field's proto2 default, so a
    // handler that only checked "did it decode" would call this a success.
    let err = answer_request(
        EMSG_AM_CLIENT_SET_PLAYER_NICKNAME,
        Vec::new(),
        None,
        |h| async move { friends::set_nickname(&h, OTHER_ID, "rival").await },
    )
    .await
    .expect_err("a missing eresult is Fail, not OK");
    assert_remote(&err, "eresult 2");
}

#[tokio::test]
async fn delete_friends_group_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_AM_CLIENT_DELETE_FRIENDS_GROUP,
        CMsgClientDeleteFriendsGroupResponse {
            eresult: Some(ERESULT_LIMIT_EXCEEDED),
        }
        .encode_to_vec(),
        None,
        |h| async move { friends::delete_friends_group(&h, 7).await },
    )
    .await
    .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
}

#[tokio::test]
async fn rename_friends_group_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_AM_CLIENT_MANAGE_FRIENDS_GROUP,
        CMsgClientManageFriendsGroupResponse {
            eresult: Some(ERESULT_LIMIT_EXCEEDED),
        }
        .encode_to_vec(),
        None,
        |h| async move { friends::rename_friends_group(&h, 7, "squad").await },
    )
    .await
    .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
}

#[tokio::test]
async fn remove_friends_from_group_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_AM_CLIENT_REMOVE_FRIEND_FROM_GROUP,
        CMsgClientRemoveFriendFromGroupResponse {
            eresult: Some(ERESULT_LIMIT_EXCEEDED),
        }
        .encode_to_vec(),
        None,
        |h| async move { friends::remove_friends_from_group(&h, 7, &[OTHER_ID]).await },
    )
    .await
    .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
}

#[tokio::test]
async fn add_friends_to_group_stops_at_the_first_rejection() {
    let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
    let task = tokio::spawn(async move {
        friends::add_friends_to_group(&handle, 7, &[OTHER_ID, OTHER_ID + 1]).await
    });

    match commands.recv().await.expect("first request queued") {
        Command::Request { emsg, reply, .. } => {
            assert_eq!(emsg, EMSG_AM_CLIENT_ADD_FRIEND_TO_GROUP);
            reply
                .send(Ok(SteamMessage {
                    emsg,
                    header: CMsgProtoBufHeader::default(),
                    body: CMsgClientAddFriendToGroupResponse {
                        eresult: Some(ERESULT_LIMIT_EXCEEDED),
                    }
                    .encode_to_vec(),
                }))
                .expect("reply delivered");
        }
        _ => panic!("expected a Request"),
    }

    let err = task
        .await
        .expect("op task")
        .expect_err("a rejection must not read as success");
    assert_remote(&err, "eresult 25");
    assert!(
        commands.try_recv().is_err(),
        "the second steam id must not be sent after a rejection"
    );
}

#[tokio::test]
async fn a_non_ok_header_eresult_fails_the_request() {
    // The body says OK; the routing header says no. The header wins, otherwise
    // a Steam-side rejection with an empty body reads as a success.
    let err = answer_request(
        EMSG_AM_CLIENT_SET_PLAYER_NICKNAME,
        CMsgClientSetPlayerNicknameResponse { eresult: Some(1) }.encode_to_vec(),
        Some(15), // EResult::AccessDenied
        |h| async move { friends::set_nickname(&h, OTHER_ID, "rival").await },
    )
    .await
    .expect_err("a header rejection must not read as success");
    assert_remote(&err, "eresult 15");
}

// ---- friends: the post-login snapshot cache -------------------------------

#[tokio::test]
async fn request_friends_list_reads_the_cached_snapshot() {
    let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
    let body = CMsgClientFriendsList {
        bincremental: Some(false),
        friends: vec![ProtoFriend {
            ulfriendid: Some(OTHER_ID),
            efriendrelationship: Some(3), // Friend
        }],
        ..Default::default()
    }
    .encode_to_vec();
    snapshots
        .lock()
        .expect("snapshot cache mutex")
        .insert(EMSG_CLIENT_FRIENDS_LIST, body);

    let list = friends::request_friends_list(&handle)
        .await
        .expect("the cached snapshot answers without a push");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].steam_id, OTHER_ID);
    assert_eq!(list[0].relationship, friends::FriendRelationship::Friend);
}

#[tokio::test]
async fn request_friends_list_skips_an_incremental_cached_delta() {
    let (handle, _commands, events, snapshots) = SessionHandle::for_test(SELF_ID);
    snapshots.lock().expect("snapshot cache mutex").insert(
        EMSG_CLIENT_FRIENDS_LIST,
        CMsgClientFriendsList {
            bincremental: Some(true),
            friends: vec![ProtoFriend {
                ulfriendid: Some(999),
                efriendrelationship: Some(3),
            }],
            ..Default::default()
        }
        .encode_to_vec(),
    );

    let task = tokio::spawn(async move { friends::request_friends_list(&handle).await });
    let full = SteamMessage {
        emsg: EMSG_CLIENT_FRIENDS_LIST,
        header: CMsgProtoBufHeader::default(),
        body: CMsgClientFriendsList {
            bincremental: Some(false),
            friends: vec![ProtoFriend {
                ulfriendid: Some(OTHER_ID),
                efriendrelationship: Some(3),
            }],
            ..Default::default()
        }
        .encode_to_vec(),
    };
    await_subscriber(&events).await;
    events.send(full).expect("push delivered");

    let list = task.await.expect("op task").expect("the full list arrives");
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].steam_id, OTHER_ID,
        "the incremental delta must not answer the request"
    );
}

// ---- licenses: the post-login snapshot cache -------------------------------

#[tokio::test]
async fn licenses_reads_the_cached_snapshot() {
    let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
    let body = CMsgClientLicenseList {
        licenses: vec![ProtoLicense {
            package_id: Some(730),
            time_created: Some(1_700_000_000),
            payment_method: Some(1),
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec();
    snapshots
        .lock()
        .expect("snapshot cache mutex")
        .insert(EMSG_CLIENT_LICENSE_LIST, body);

    let list = licenses::licenses(&handle).expect("the cached snapshot answers without a push");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].package_id, 730);
    assert_eq!(licenses::owns_package(&handle, 730), Some(true));
    assert_eq!(licenses::owns_package(&handle, 999), Some(false));
}

#[tokio::test]
async fn licenses_is_none_before_any_push() {
    let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
    assert!(licenses::licenses(&handle).is_none());
    assert!(licenses::owns_package(&handle, 730).is_none());
}

// ---- persona --------------------------------------------------------------

/// Spawn `request_player_summary`, returning its task plus the event sender,
/// once the request it emits has been asserted and acked.
async fn start_player_summary() -> (
    tokio::task::JoinHandle<Result<persona::PlayerSummary>>,
    broadcast::Sender<SteamMessage>,
) {
    let (handle, mut commands, events, _snapshots) = SessionHandle::for_test(SELF_ID);
    let task =
        tokio::spawn(async move { persona::request_player_summary(&handle, OTHER_ID).await });
    match commands.recv().await.expect("notify queued") {
        Command::Notify { emsg, body, ack } => {
            assert_eq!(emsg, EMSG_CLIENT_REQUEST_FRIEND_DATA);
            let sent = CMsgClientRequestFriendData::decode(body.as_slice()).expect("decodes");
            assert_eq!(sent.friends, vec![OTHER_ID]);
            let flags = sent.persona_state_requested.expect("flags requested");
            assert_eq!(flags & 1, 1, "Status bit must be requested: {flags}");
            assert_eq!(flags & 2, 2, "PlayerName bit must be requested: {flags}");
            ack.send(Ok(())).expect("ack delivered");
        }
        _ => panic!("expected a Notify"),
    }
    (task, events)
}

fn persona_state(status_flags: u32, friend: Persona) -> SteamMessage {
    SteamMessage {
        emsg: EMSG_CLIENT_PERSONA_STATE,
        header: CMsgProtoBufHeader::default(),
        body: CMsgClientPersonaState {
            status_flags: Some(status_flags),
            friends: vec![friend],
        }
        .encode_to_vec(),
    }
}

#[tokio::test]
async fn request_player_summary_ignores_a_partial_persona_state() {
    let (mut task, events) = start_player_summary().await;

    // Status only: a "went online" push, with no player name in it.
    events
        .send(persona_state(
            1,
            Persona {
                friendid: Some(OTHER_ID),
                persona_state: Some(1),
                ..Default::default()
            },
        ))
        .expect("partial push delivered");
    // Also a full state, but for somebody else.
    events
        .send(persona_state(
            3,
            Persona {
                friendid: Some(SELF_ID),
                player_name: Some("someone else".into()),
                ..Default::default()
            },
        ))
        .expect("other-account push delivered");

    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut task)
            .await
            .is_err(),
        "neither push answers the request, so it must still be waiting"
    );

    events
        .send(persona_state(
            3,
            Persona {
                friendid: Some(OTHER_ID),
                persona_state: Some(1),
                player_name: Some("roidbot".into()),
                avatar_hash: Some(vec![0xde, 0xad, 0xbe, 0xef]),
                ..Default::default()
            },
        ))
        .expect("full push delivered");

    let summary = task
        .await
        .expect("op task")
        .expect("the full state answers");
    assert_eq!(summary.steam_id, OTHER_ID);
    assert_eq!(summary.persona_name, "roidbot");
    assert_eq!(summary.avatar_hash, "deadbeef");
    assert!(summary.current_game.is_none());
}

#[tokio::test]
async fn request_profile_info_rejects_a_non_ok_eresult() {
    let err = answer_request(
        EMSG_CLIENT_FRIEND_PROFILE_INFO,
        CMsgClientFriendProfileInfoResponse {
            eresult: Some(15), // AccessDenied, a private profile
            real_name: Some("Gabe".into()),
            ..Default::default()
        }
        .encode_to_vec(),
        None,
        |h| async move { persona::request_profile_info(&h, OTHER_ID).await },
    )
    .await
    .expect_err("a refused profile must not read as an empty one");
    assert_remote(&err, "eresult 15");
}

#[tokio::test]
async fn request_profile_info_maps_an_ok_response() {
    let info = answer_request(
        EMSG_CLIENT_FRIEND_PROFILE_INFO,
        CMsgClientFriendProfileInfoResponse {
            eresult: Some(1),
            steamid_friend: Some(OTHER_ID),
            real_name: Some("Gabe".into()),
            city_name: Some(String::new()),
            time_created: Some(1_234),
            ..Default::default()
        }
        .encode_to_vec(),
        None,
        |h| async move { persona::request_profile_info(&h, OTHER_ID).await },
    )
    .await
    .expect("eresult OK is a success");
    assert_eq!(info.steam_id, OTHER_ID);
    assert_eq!(info.real_name.as_deref(), Some("Gabe"));
    assert_eq!(info.city, None, "an empty string means not shared");
    assert_eq!(info.created_at, Some(1_234));
}

// ---- wallet: last-wins cache -----------------------------------------------

fn wallet_push(balance64: i64, currency: i32) -> Vec<u8> {
    CMsgClientWalletInfoUpdate {
        has_wallet: Some(true),
        balance64: Some(balance64),
        currency: Some(currency),
        ..Default::default()
    }
    .encode_to_vec()
}

#[tokio::test]
async fn wallet_is_none_before_any_push() {
    let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
    assert!(wallet::wallet(&handle).is_none());
}

#[tokio::test]
async fn wallet_reads_the_cached_push() {
    let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
    snapshots
        .lock()
        .expect("snapshot cache mutex")
        .insert(EMSG_CLIENT_WALLET_INFO_UPDATE, wallet_push(2_500, 1));

    let w = wallet::wallet(&handle).expect("cached push answers");
    assert_eq!(w.balance_minor_units, 2_500);
    assert_eq!(w.currency, 1);
}

#[tokio::test]
async fn wallet_reflects_a_second_push_replacing_the_first() {
    let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
    snapshots
        .lock()
        .expect("snapshot cache mutex")
        .insert(EMSG_CLIENT_WALLET_INFO_UPDATE, wallet_push(1_000, 1));
    assert_eq!(
        wallet::wallet(&handle)
            .expect("first push answers")
            .balance_minor_units,
        1_000
    );

    // A balance change re-pushes; wallet() must reflect the *latest* value,
    // the opposite of the friends post-login snapshot, which is deliberately
    // first-wins (see `dispatch_caches_the_latest_wallet_push_last_wins` and
    // `dispatch_caches_first_snapshot_per_emsg` in `session::driver`'s own
    // tests for the mechanism this exercises from the public side).
    snapshots
        .lock()
        .expect("snapshot cache mutex")
        .insert(EMSG_CLIENT_WALLET_INFO_UPDATE, wallet_push(750, 1));

    assert_eq!(
        wallet::wallet(&handle)
            .expect("second push answers")
            .balance_minor_units,
        750,
        "the second push must replace the first"
    );
}

// ---- the seam itself ------------------------------------------------------

#[tokio::test]
async fn a_stopped_driver_fails_the_request() {
    let (handle, commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
    drop(commands);
    let err = friends::set_nickname(&handle, OTHER_ID, "rival")
        .await
        .expect_err("no driver, no success");
    assert!(matches!(err, Error::WebSocket(_)), "{err:?}");
}
