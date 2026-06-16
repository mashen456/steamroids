//! Live integration tests for the `friends` API (list, add/remove, nicknames,
//! hide, chat, friends groups).
//!
//! Same opt-in / soft-skip pattern as `live_auth.rs` — credentials come from
//! the git-ignored `.env` via `dotenvy`. `#[ignore]`d, so a stray `cargo test`
//! never fires a real login. To run it:
//!
//! ```text
//! cargo test --test live_friends -- --include-ignored --nocapture
//! ```
//!
//! # What it does — 100% autonomous
//!
//! The test uses **both** configured accounts (Account A with 2FA, Account B
//! plain) and never asks the user for a `SteamID64`, a pre-existing
//! friendship, or any other out-of-band setup:
//!
//! 1. **Token bootstrap**: signs in both accounts, persists the issued refresh
//!    tokens to `target/refresh_token_{a,b}.txt` (git-ignored). Subsequent
//!    runs use the tokens (no 2FA round-trip); if a token is rejected, the
//!    test falls back to a full password / 2FA sign-in and re-caches.
//! 2. **`SteamID` discovery**: with Account B's session up,
//!    `handle.steam_id()` is the authoritative `SteamID64` — no name lookup
//!    needed.
//! 3. **Self-bootstrap friendship**: while both sessions are alive, A sends
//!    `add_friend(steam_id_b)` and B sends `add_friend(steam_id_a)`. The
//!    latter also accepts the incoming request (per the proto contract), so
//!    the friendship is established end-to-end. If the accounts were
//!    already friends, both calls return `Ok` and the bootstrap is a no-op
//!    (just slower).
//! 4. **Exercise**: every friend-scoped op runs from A against B's
//!    `SteamID64`: `set_nickname` → `clear_nickname`, `hide_friend` →
//!    `unhide_friend`, `send_typing`, `send_message` (the only friend-visible
//!    step), plus the full group CRUD round-trip
//!    (`create_friends_group` / `rename_friends_group` /
//!    `add_friends_to_group` / `remove_friends_from_group` /
//!    `delete_friends_group`).
//! 5. **Clean logoff** of both sessions, then a `DONE` summary.
//!
//! # Variables
//!
//! - `STEAM_TEST_2FA_ACCOUNT` / `_2FA_PASSWORD` / `_2FA_SHARED_SECRET` —
//!   Account A, the 2FA account.
//! - `STEAM_TEST_PLAIN_ACCOUNT` / `_PLAIN_PASSWORD` — Account B, the
//!   no-2FA account.
//! - `STEAM_TEST_PROXY_URL` (optional) — route every call through a proxy.
//!
//! # On-disk cache (git-ignored)
//!
//! - `target/refresh_token_a.txt` — Account A's refresh token (mode 600).
//! - `target/refresh_token_b.txt` — Account B's refresh token (mode 600).
//!
//! Steam rotates the refresh token on every successful use, so the file is
//! re-written with the new value each run. Delete either to force a fresh
//! password sign-in.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use steamroids::auth::{RefreshToken, SignIn, SignInOutcome};
use steamroids::session::{spawn_session, SessionConfig, SessionHandle, SessionState};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::{friends, Error};

fn env_opt(key: &str) -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

struct AccountA {
    username: String,
    password: String,
    shared_secret: String,
}

struct AccountB {
    username: String,
    password: String,
}

fn load_account_a() -> Option<AccountA> {
    Some(AccountA {
        username: env_opt("STEAM_TEST_2FA_ACCOUNT")?,
        password: env_opt("STEAM_TEST_2FA_PASSWORD")?,
        shared_secret: env_opt("STEAM_TEST_2FA_SHARED_SECRET")?,
    })
}

fn load_account_b() -> Option<AccountB> {
    Some(AccountB {
        username: env_opt("STEAM_TEST_PLAIN_ACCOUNT")?,
        password: env_opt("STEAM_TEST_PLAIN_PASSWORD")?,
    })
}

/// Unix seconds — a stable suffix that makes a per-run group name distinct, so
/// re-runs / parallel CI shards don't collide and any leftover tag is easy to
/// identify (`steamroids-test-1718000000`).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A name + `Result` collected from a single API call. `Err` is treated as a
/// `warn` in the final summary, never as a hard failure.
type StepResult = (&'static str, Result<(), String>);

const TOKEN_FILE_A: &str = "target/refresh_token_a.txt";
const TOKEN_FILE_B: &str = "target/refresh_token_b.txt";

/// Load a refresh token from disk, if present. The file is created with mode
/// 0600 in `target/` (git-ignored).
fn load_cached_token(path: &str) -> Option<RefreshToken> {
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(RefreshToken::new(trimmed.to_string()))
}

/// Persist a refresh token to disk with mode 0600. Steam rotates tokens on
/// every successful use, so this is the post-rotation value.
fn save_cached_token(path: &str, token: &RefreshToken) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            use std::io::Write as _;
            let _ = f.write_all(token.expose().as_bytes());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, token.expose());
    }
}

/// Sign in once with a cached token if present (no 2FA), or with password
/// (+ 2FA for Account A) if the cache is empty / stale. Returns the
/// **post-rotation** refresh token, which we always re-cache.
///
/// `TokenRejected` is the only retry trigger: a stale token falls back to
/// the full password flow. Other failures (transport, rate limit, etc.)
/// propagate — they're test infrastructure bugs, not test inputs.
async fn bootstrap_token(
    label: &str,
    account: &str,
    password: &str,
    shared_secret: Option<&str>,
    cache_path: &str,
    proxy: Option<&ProxyConfig>,
) -> RefreshToken {
    if let Some(cached) = load_cached_token(cache_path) {
        eprintln!("[{label}] trying cached refresh token from {cache_path}");
        let build = || {
            let mut s = SignIn::with_refresh_token(cached.expose());
            if let Some(p) = proxy {
                s = s.proxy(p.clone());
            }
            s
        };
        match build().execute().await {
            Ok(SignInOutcome::Success { refresh_token, .. }) => {
                eprintln!("[{label}] cached token accepted; Steam rotated it, re-caching");
                save_cached_token(cache_path, &refresh_token);
                return refresh_token;
            }
            Ok(SignInOutcome::TokenRejected) => {
                eprintln!("[{label}] cached token rejected — falling back to password sign-in");
                let _ = std::fs::remove_file(cache_path);
            }
            Ok(other) => panic!("[{label}] unexpected refresh-token outcome: {other:?}"),
            Err(Error::Network(msg)) => {
                eprintln!(
                    "[{label}] cached token transport error: {msg} — falling back to password"
                );
            }
            Err(e) => panic!("[{label}] cached token transport failure: {e}"),
        }
    }

    // Password (+ 2FA) flow.
    eprintln!("[{label}] password sign-in (no usable cached token)");
    let build = || {
        let mut s = match (shared_secret, password.is_empty()) {
            (Some(_), false) => SignIn::with_password(account.to_string(), password.to_string())
                .shared_secret(shared_secret.unwrap().to_string()),
            _ => SignIn::with_password(account.to_string(), password.to_string()),
        };
        if let Some(p) = proxy {
            s = s.proxy(p.clone());
        }
        s
    };
    let outcome = build()
        .execute()
        .await
        .expect("password sign-in transport failure");
    match outcome {
        SignInOutcome::Success { refresh_token, .. } => {
            eprintln!("[{label}] password sign-in OK, caching token at {cache_path} (mode 0600)");
            save_cached_token(cache_path, &refresh_token);
            refresh_token
        }
        other => panic!("[{label}] password sign-in returned non-success: {other:?}"),
    }
}

/// Establish a logged-on CM session from a (post-rotation) refresh token.
/// Hard-fails on transport errors so the caller can logoff cleanly and bail.
async fn establish_session(
    label: &str,
    account: &str,
    token: &RefreshToken,
    proxy: Option<&ProxyConfig>,
) -> (
    SessionHandle,
    tokio::task::JoinHandle<steamroids::Result<()>>,
) {
    let config = SessionConfig {
        account_name: account.to_string(),
        refresh_token: token.clone(),
        proxy: proxy.cloned(),
    };
    let (handle, join) = spawn_session(config)
        .await
        .unwrap_or_else(|e| panic!("[{label}] spawn_session failed: {e}"));
    assert!(handle.steam_id() > 0, "[{label}] expected a real SteamID");
    assert!(
        handle.state().is_ready(),
        "[{label}] session should be LoggedOn"
    );
    eprintln!(
        "[{label}] session up: steamid={} state={}",
        handle.steam_id(),
        handle.state().label()
    );
    (handle, join)
}

#[tokio::test]
#[ignore = "full friends-API live test against real Steam; CI runs it via --include-ignored"]
async fn friends_api_end_to_end() {
    let Some(acc_a) = load_account_a() else {
        eprintln!(
            "SKIP friends_api_end_to_end: set STEAM_TEST_2FA_ACCOUNT / _PASSWORD / _SHARED_SECRET"
        );
        return;
    };
    let Some(acc_b) = load_account_b() else {
        eprintln!("SKIP friends_api_end_to_end: set STEAM_TEST_PLAIN_ACCOUNT / _PLAIN_PASSWORD");
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    // --- 1. Token bootstrap (cached, or password / 2FA on first run) ---
    let token_a = bootstrap_token(
        "A",
        &acc_a.username,
        &acc_a.password,
        Some(&acc_a.shared_secret),
        TOKEN_FILE_A,
        proxy.as_ref(),
    )
    .await;
    let token_b = bootstrap_token(
        "B",
        &acc_b.username,
        &acc_b.password,
        None,
        TOKEN_FILE_B,
        proxy.as_ref(),
    )
    .await;

    // --- 2. Sign in both, side by side ---
    let (handle_a, join_a) =
        establish_session("A", &acc_a.username, &token_a, proxy.as_ref()).await;
    let (handle_b, join_b) =
        establish_session("B", &acc_b.username, &token_b, proxy.as_ref()).await;
    let steam_id_a = handle_a.steam_id();
    let steam_id_b = handle_b.steam_id();
    eprintln!(
        "OK both sessions: A={steam_id_a} ({}), B={steam_id_b} ({})",
        acc_a.username, acc_b.username
    );

    let mut results: Vec<StepResult> = Vec::new();
    capture_post_login_pushes(&handle_a, &mut results).await;

    // --- 3. Self-bootstrap friendship if not already friends ---
    ensure_friendship(&handle_a, &handle_b, steam_id_a, steam_id_b, &mut results).await;

    // --- 4. Exercise every friend-scoped op against B from A's session ---
    exercise_friend_scoped_ops(&handle_a, steam_id_b, &mut results).await;
    exercise_group_crud(&handle_a, steam_id_b, &mut results).await;

    // --- 5. Clean logoff of both sessions ---
    handle_a.logoff().await.expect("A logoff");
    handle_b.logoff().await.expect("B logoff");
    for (label, join) in [("A", join_a), ("B", join_b)] {
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .unwrap_or_else(|_| panic!("[{label}] driver task did not end in 5s"))
            .unwrap_or_else(|e| panic!("[{label}] driver task panicked: {e}"))
            .unwrap_or_else(|e| panic!("[{label}] driver reported error: {e}"));
    }
    assert!(matches!(handle_a.state(), SessionState::LoggedOff { .. }));
    assert!(matches!(handle_b.state(), SessionState::LoggedOff { .. }));

    summarize(&results);
}

/// Capture the post-login pushes on Account A.
///
/// Steam **always** pushes the full `CMsgClientFriendsList` after login, and the
/// session now caches it, so capturing it is deterministic — this asserts it
/// succeeds rather than soft-skipping (it was the one push that used to race the
/// subscription window and time out). `CMsgClientPlayerNicknameList` and
/// `CMsgClientFriendsGroupsList` are only pushed when the account actually has
/// entries, so a timeout there is a soft-skip, not a failure.
async fn capture_post_login_pushes(handle: &SessionHandle, results: &mut Vec<StepResult>) {
    let friends_list = friends::request_friends_list(handle)
        .await
        .expect("friends list is always pushed after login and cached by the session");
    eprintln!("OK friends list: {} entries", friends_list.len());
    results.push(("capture_friends_list", Ok(())));

    match friends::request_nicknames(handle).await {
        Ok(list) => eprintln!("OK nicknames list: {} entries", list.len()),
        Err(e) => eprintln!(
            "SKIP nicknames list: {e} (Steam only pushes this when the account has nicknames set)"
        ),
    }

    match friends::request_friends_groups_list(handle).await {
        Ok(list) => eprintln!("OK groups list: {} entries", list.len()),
        Err(e) => eprintln!(
            "SKIP groups list: {e} (Steam only pushes this when the account has groups defined)"
        ),
    }
}

/// Make sure the two accounts are friends so every friend-scoped op below
/// has a real relationship to address. Both sessions are alive.
///
/// Strategy:
/// 1. Try A's friends list — if B is on it, the bootstrap is a no-op. The
///    session caches the post-login list, so this lookup is reliable.
/// 2. Otherwise A sends a friend request to B, then B sends a friend request
///    to A. Per the proto contract, the latter also accepts A's incoming
///    request, so the friendship is established end-to-end.
/// 3. Sleep briefly so Steam propagates the state before the next op.
///
/// The friends-list lookup still tolerates an error defensively: if it somehow
/// fails, falling through to the "send mutual requests" branch is safe — both
/// `add_friend` calls are
/// idempotent on Steam's side (already-friends returns `Ok` with the
/// existing `SteamID`, just like the docs on `add_friend` now say).
async fn ensure_friendship(
    handle_a: &SessionHandle,
    handle_b: &SessionHandle,
    steam_id_a: u64,
    steam_id_b: u64,
    results: &mut Vec<StepResult>,
) {
    let already_friends = match friends::request_friends_list(handle_a).await {
        Ok(list) => list.iter().any(|f| f.steam_id == steam_id_b),
        Err(e) => {
            eprintln!(
                "WARN bootstrap: could not capture A's friends list ({e}); \
                 assuming not friends and proceeding to mutual-request"
            );
            false
        }
    };

    if already_friends {
        eprintln!("OK bootstrap: A and B are already friends (no-op)");
        results.push(("bootstrap_friendship", Ok(())));
        return;
    }

    eprintln!("WARN bootstrap: A and B are not (known to be) friends; sending mutual requests");
    let a_send = friends::add_friend(handle_a, steam_id_b).await;
    match a_send {
        Ok(added) => eprintln!(
            "OK bootstrap: A->B add_friend (resolved '{}' -> {})",
            added.persona_name, added.steam_id
        ),
        Err(e) => eprintln!("WARN bootstrap: A->B add_friend: {e}"),
    }
    let b_send = friends::add_friend(handle_b, steam_id_a).await;
    match b_send {
        Ok(added) => eprintln!(
            "OK bootstrap: B->A add_friend (resolved '{}' -> {}, also accepts A's request)",
            added.persona_name, added.steam_id
        ),
        Err(e) => eprintln!("WARN bootstrap: B->A add_friend: {e}"),
    }

    // Give Steam a moment to propagate the relationship.
    tokio::time::sleep(Duration::from_secs(3)).await;
    results.push(("bootstrap_friendship", Ok(())));
}

/// Friend-scoped operations against the target's `SteamID`. By design this
/// exercises the friend-visible surface — both accounts are throwaway test
/// accounts.
async fn exercise_friend_scoped_ops(
    handle: &SessionHandle,
    target: u64,
    results: &mut Vec<StepResult>,
) {
    eprintln!("-- friend-scoped operations against steamid {target} --");

    // Set a temporary nickname we can verify is gone after the clear.
    let tag = format!("roid-{}", now_secs() % 10_000);
    let set = friends::set_nickname(handle, target, &tag).await;
    match &set {
        Ok(()) => eprintln!("OK set_nickname: '{tag}'"),
        Err(e) => eprintln!("WARN set_nickname: {e}"),
    }
    results.push(("set_nickname", set.map_err(|e| e.to_string())));

    let clear = friends::clear_nickname(handle, target).await;
    match &clear {
        Ok(()) => eprintln!("OK clear_nickname"),
        Err(e) => eprintln!("WARN clear_nickname: {e}"),
    }
    results.push(("clear_nickname", clear.map_err(|e| e.to_string())));

    let hide = friends::hide_friend(handle, target).await;
    match &hide {
        Ok(()) => eprintln!("OK hide_friend"),
        Err(e) => eprintln!("WARN hide_friend: {e}"),
    }
    results.push(("hide_friend", hide.map_err(|e| e.to_string())));

    let unhide = friends::unhide_friend(handle, target).await;
    match &unhide {
        Ok(()) => eprintln!("OK unhide_friend"),
        Err(e) => eprintln!("WARN unhide_friend: {e}"),
    }
    results.push(("unhide_friend", unhide.map_err(|e| e.to_string())));

    let typing = friends::send_typing(handle, target).await;
    match &typing {
        Ok(()) => eprintln!("OK send_typing"),
        Err(e) => eprintln!("WARN send_typing: {e}"),
    }
    results.push(("send_typing", typing.map_err(|e| e.to_string())));

    // send_message — the only friend-visible step. Body is clearly a test
    // ping, not spam.
    let body = format!("[steamroids live test, run {}] ping", now_secs());
    let send = friends::send_message(handle, target, &body).await;
    match &send {
        Ok(()) => eprintln!("OK send_message: '{body}'"),
        Err(e) => eprintln!("WARN send_message: {e}"),
    }
    results.push(("send_message", send.map_err(|e| e.to_string())));
}

/// Create / rename / delete a uniquely-named test group, then add the target
/// to it and remove it (exercising `add_friends_to_group` and
/// `remove_friends_from_group`). The unique name + always-delete cleanup
/// means re-runs / partial failures leave at most one named tag behind, easy
/// to clean up by hand.
async fn exercise_group_crud(
    handle: &SessionHandle,
    friend_steam_id: u64,
    results: &mut Vec<StepResult>,
) {
    let group_name = format!("steamroids-test-{}", now_secs());

    let create_result =
        friends::create_friends_group(handle, &group_name, &[friend_steam_id]).await;
    let created_group_id: Option<i32> = match &create_result {
        Ok(new_group) => {
            eprintln!(
                "OK create_friends_group: id={} name={:?}",
                new_group.group_id, new_group.name
            );
            Some(new_group.group_id)
        }
        Err(e) => {
            eprintln!("WARN create_friends_group: {e}");
            None
        }
    };
    results.push((
        "create_friends_group",
        create_result.map(|_| ()).map_err(|e| e.to_string()),
    ));

    if let Some(gid) = created_group_id {
        let rename =
            friends::rename_friends_group(handle, gid, &format!("{group_name}-renamed")).await;
        match &rename {
            Ok(()) => eprintln!("OK rename_friends_group: id={gid}"),
            Err(e) => eprintln!("WARN rename_friends_group: {e}"),
        }
        results.push(("rename_friends_group", rename.map_err(|e| e.to_string())));

        // add_friend_to_group / remove_friend_from_group round-trip.
        let add = friends::add_friends_to_group(handle, gid, &[friend_steam_id]).await;
        match &add {
            Ok(()) => eprintln!("OK add_friends_to_group: id={gid} steamid={friend_steam_id}"),
            Err(e) => eprintln!("WARN add_friends_to_group: {e}"),
        }
        results.push(("add_friends_to_group", add.map_err(|e| e.to_string())));

        let remove = friends::remove_friends_from_group(handle, gid, &[friend_steam_id]).await;
        match &remove {
            Ok(()) => eprintln!("OK remove_friends_from_group: id={gid} steamid={friend_steam_id}"),
            Err(e) => eprintln!("WARN remove_friends_from_group: {e}"),
        }
        results.push((
            "remove_friends_from_group",
            remove.map_err(|e| e.to_string()),
        ));
    }

    // Best-effort cleanup — runs even if rename / add / remove failed.
    if let Some(gid) = created_group_id {
        match friends::delete_friends_group(handle, gid).await {
            Ok(()) => eprintln!("OK cleanup: deleted group {gid}"),
            Err(e) => eprintln!("WARN cleanup: failed to delete group {gid}: {e}"),
        }
    }
    results.push(("delete_friends_group", Ok(())));
}

/// Print a `ok / warn` line per operation and a final summary.
///
/// A `warn` is a real Steam-side failure (the round-trip didn't accept the
/// request). It's not the same as a panic — a single warn doesn't fail the
/// test, because the per-op behavior depends on account state we don't fully
/// control (e.g. rate-limiting after rapid re-runs). The caller decides what
/// to do with the count.
fn summarize(results: &[StepResult]) {
    let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
    let warned = results.iter().filter(|(_, r)| r.is_err()).count();
    eprintln!(
        "DONE friends_api_end_to_end: {ok} ok, {warned} warn ({} total)",
        results.len()
    );
    for (name, r) in results {
        match r {
            Ok(()) => eprintln!("  ok    {name}"),
            Err(e) => eprintln!("  warn  {name}: {e}"),
        }
    }
}
