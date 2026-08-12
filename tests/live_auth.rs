//! Live integration tests that authenticate against real Steam.
//!
//! These hit `api.steampowered.com` with real credentials, so they are
//! **opt-in** and driven by environment variables — loaded locally from a
//! git-ignored `.env` (via `dotenvy`) or, in CI, from encrypted secrets. When
//! the variables for a test are absent it prints a `SKIP` notice and returns
//! instead of failing, so the normal `cargo test` run, contributor machines,
//! and fork PRs stay green without credentials.
//!
//! Set `STEAM_LIVE_REQUIRED=1` to turn every one of those soft-skips into a
//! panic. CI's live job sets it, so a missing secret fails the job instead of
//! passing having tested nothing.
//!
//! # Accounts under test
//!
//! - **2FA account** (`STEAM_TEST_2FA_*`) — password plus a mobile
//!   authenticator shared secret.
//! - **Plain account** (`STEAM_TEST_PLAIN_*`) — password only. For a green
//!   `login OK` this account must have Steam Guard fully disabled; if it still
//!   has email Guard the test soft-skips (our flow can't enter an email code
//!   yet). This account also drives the CS2 Game Coordinator scan
//!   (`cs2_profile_scan`), the web-session test
//!   (`web_session_authenticates_a_community_request`), and the GCPD cooldown
//!   fetch (`web_session_fetches_the_cs2_cooldown`).
//! - **CS2 account** (`STEAM_TEST_CS2_*`, optional) — an account that owns / has
//!   launched CS2, so its Game Coordinator welcomes us. Drives the real profile
//!   scan; if unset, `cs2_profile_scan` falls back to the plain account and
//!   soft-skips when the GC stays silent. Must differ from the 2FA account.
//! - **Refresh token** (`STEAM_TEST_REFRESH_TOKEN`, optional) — exercises the
//!   token-reuse path.
//!
//! The real-login tests are `#[ignore]`d so a stray `cargo test` never fires
//! live logins; CI opts in with `-- --include-ignored`. The 2FA account is
//! logged in at most once per run, only inside `cm_logon_over_wss`, because a
//! second concurrent login would reuse the same TOTP code inside its 30s
//! window and Steam rejects the duplicate. The plain account logs in from
//! three tests (`cs2_profile_scan`, `web_session_authenticates_a_community_request`,
//! and `web_session_fetches_the_cs2_cooldown`). It carries no shared secret,
//! so there is no one-time code for the logins to collide on, but concurrent
//! logins would still open concurrent CM sessions on the same account, and
//! Steam evicts one of them. CI serializes this binary (`--test-threads=1`)
//! to avoid that.
//!
//! # Running locally
//!
//! Copy `.env.example` to `.env`, fill in *throwaway* accounts, then:
//!
//! ```text
//! cargo test --test live_auth -- --include-ignored --nocapture
//! ```
//!
//! # Variables
//!
//! - `STEAM_TEST_2FA_ACCOUNT` / `_2FA_PASSWORD` / `_2FA_SHARED_SECRET`
//! - `STEAM_TEST_PLAIN_ACCOUNT` / `_PLAIN_PASSWORD`
//! - `STEAM_TEST_CS2_ACCOUNT` / `_CS2_PASSWORD` / `_CS2_SHARED_SECRET` (optional)
//! - `STEAM_TEST_CS2_TARGET_ID` (optional) — scan this `SteamID64` instead of self
//! - `STEAM_TEST_REFRESH_TOKEN` (optional)
//! - `STEAM_TEST_PROXY_URL` (optional) — routes every call through a proxy
//! - `STEAM_LIVE_REQUIRED` (optional) — any value but `0` makes a skip a panic

use std::time::Duration;

use steamroids::auth::{RefreshToken, SignIn, SignInOutcome};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::Error;

/// Where `cm_logon_over_wss` parks Account A's refresh token so the friends live
/// test — a separate binary, run as the next CI step — can reuse it instead of
/// doing a second 2FA login in the same 30s TOTP window, which Steam rejects as
/// a duplicate code. Same path `tests/live_friends.rs` reads from, so the
/// handoff is just "first test writes, second test reads."
const TOKEN_FILE_A: &str = "target/refresh_token_a.txt";

/// Persist a refresh token with mode 0600 (best-effort — on failure the consumer
/// just falls back to its own login).
fn persist_token(path: &str, token: &RefreshToken) {
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

/// Read an environment variable, loading `.env` first (best-effort). Returns
/// `None` for missing *or* empty values — CI passes unset secrets through as
/// empty strings, and an empty value must count as "not provided" so the test
/// soft-skips rather than sending garbage to Steam.
fn env_opt(key: &str) -> Option<String> {
    // `dotenvy` is a no-op when there is no `.env`; calling it here keeps the
    // helper self-contained and is cheap enough for a handful of lookups.
    let _ = dotenvy::dotenv();
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Whether live coverage is mandatory. `STEAM_LIVE_REQUIRED` set to anything
/// but `0` means the caller wants real coverage, so a skip is a failure.
fn live_required() -> bool {
    env_opt("STEAM_LIVE_REQUIRED").is_some_and(|v| v != "0")
}

/// Print a `SKIP` notice, or panic when `required`. Split from `skip` so the
/// decision is testable without touching the environment.
fn note_skip(required: bool, reason: &str) {
    assert!(
        !required,
        "STEAM_LIVE_REQUIRED is set but the live test skipped: {reason}"
    );
    eprintln!("SKIP {reason}");
}

/// Print a `SKIP` notice, or panic when `STEAM_LIVE_REQUIRED` is set. Callers
/// still `return` afterwards; this only decides skip-vs-fail.
fn skip(reason: &str) {
    note_skip(live_required(), reason);
}

/// Route the builder through `STEAM_TEST_PROXY_URL` if it is set. A malformed
/// URL panics — that is a test-setup error, not a reason to skip silently.
fn with_optional_proxy(signin: SignIn) -> SignIn {
    match env_opt("STEAM_TEST_PROXY_URL") {
        Some(url) => {
            let proxy =
                ProxyConfig::parse(&url).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL");
            eprintln!("-> routing live test through proxy {}", proxy.display());
            signin.proxy(proxy)
        }
        None => signin,
    }
}

/// One configured test account, read from a `STEAM_TEST_<prefix>_*` group.
struct Account {
    username: String,
    password: String,
    shared_secret: Option<String>,
}

/// Load an account from the `STEAM_TEST_<prefix>_ACCOUNT` / `_PASSWORD` /
/// `_SHARED_SECRET` variables. Returns `None` if account or password is unset.
fn load_account(prefix: &str) -> Option<Account> {
    Some(Account {
        username: env_opt(&format!("STEAM_TEST_{prefix}_ACCOUNT"))?,
        password: env_opt(&format!("STEAM_TEST_{prefix}_PASSWORD"))?,
        shared_secret: env_opt(&format!("STEAM_TEST_{prefix}_SHARED_SECRET")),
    })
}

/// Execute a freshly-built `SignIn`, retrying on transient network errors.
///
/// Live tests run against real Steam through real (often rotating) proxies, so
/// a `GetPasswordRSAPublicKey`-style "error sending request" is an expected
/// blip, not a failure — exactly what a fleet client retries. `build` is called
/// once per attempt because `execute` consumes the builder.
async fn execute_with_retry(label: &str, build: impl Fn() -> SignIn) -> SignInOutcome {
    const MAX_ATTEMPTS: u32 = 4;
    // totp codes rotate every 30s; wait past the boundary for a fresh one
    const TOTP_WINDOW: Duration = Duration::from_secs(31);
    for attempt in 1..=MAX_ATTEMPTS {
        match build().execute().await {
            // same code twice in one window: steam rejects the duplicate
            Ok(SignInOutcome::GuardCodeRejected) if attempt < MAX_ATTEMPTS => {
                eprintln!(
                    "[{label}] guard code rejected (attempt {attempt}/{MAX_ATTEMPTS}), \
                     waiting for the next TOTP window"
                );
                tokio::time::sleep(TOTP_WINDOW).await;
            }
            Ok(outcome) => return outcome,
            Err(Error::Network(msg)) if attempt < MAX_ATTEMPTS => {
                eprintln!(
                    "[{label}] transient network error (attempt {attempt}/{MAX_ATTEMPTS}): {msg}"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => panic!("[{label}] sign-in failed at the transport level: {e}"),
        }
    }
    unreachable!("loop returns or panics on the final attempt");
}

// NB: the 2FA account's password login is exercised as step 1 of
// `cm_logon_over_wss`; there is no separate `login_account_with_2fa` test, so
// that account is never logged in twice in one run (concurrent logins reuse the
// same TOTP code within a 30s window and Steam rejects the duplicate).

/// Sign `acc` in for a refresh token, retrying on proxy blips. Returns `None`
/// (after printing a `SKIP`) for the soft-skippable outcomes — email Guard or
/// rate-limiting — and panics on a hard failure. Asserts a real login on
/// success, folding in the plain-account login coverage.
async fn sign_in_for_session(
    label: &str,
    acc: &Account,
    proxy: Option<&ProxyConfig>,
) -> Option<steamroids::auth::RefreshToken> {
    let outcome = execute_with_retry(label, || {
        let mut s = SignIn::with_password(acc.username.clone(), acc.password.clone());
        if let Some(secret) = &acc.shared_secret {
            s = s.shared_secret(secret.clone());
        }
        if let Some(p) = proxy {
            s = s.proxy(p.clone());
        }
        s
    })
    .await;

    match outcome {
        SignInOutcome::Success {
            steam_id,
            refresh_token,
            ..
        } => {
            assert!(steam_id > 0, "[{label}] expected a real SteamID");
            assert!(
                !refresh_token.expose().is_empty(),
                "[{label}] expected a refresh token"
            );
            eprintln!("OK [{label}] login for steam_id {steam_id}");
            Some(refresh_token)
        }
        SignInOutcome::NeedsEmailGuardCode { email_domain } => {
            skip(&format!("[{label}]: account still has email Steam Guard (domain {email_domain}); disable Steam Guard for a real login test"));
            None
        }
        SignInOutcome::RateLimited { retry_hint } => {
            skip(&format!(
                "[{label}]: Steam rate-limited (retry {retry_hint:?})"
            ));
            None
        }
        SignInOutcome::NeedsMobileGuardCode => panic!(
            "[{label}] account needs mobile 2FA — set STEAM_TEST_{}_SHARED_SECRET",
            label.to_uppercase()
        ),
        SignInOutcome::InvalidCredentials => {
            panic!("[{label}] username/password rejected by Steam")
        }
        other => panic!("[{label}] unexpected outcome: {other:?}"),
    }
}

/// Full CS2 Game Coordinator scan with the plain account: WebAPI sign-in -> CM
/// session -> "play" CS2 -> GC welcome -> request a player's profile. This is
/// the 0.3.x end-to-end path; it also covers the plain-account login (asserted
/// inside [`sign_in_for_session`]). `#[ignore]`d (real login).
///
/// Whether the GC welcomes us depends on the account having a CS2 license, which
/// is an account-provisioning detail, not a code property — so a GC that never
/// becomes ready is a loud `SKIP`, not a failure. When it *does* welcome, the
/// profile round-trip is asserted.
#[tokio::test]
#[ignore = "full CS2 GC scan against real Steam; CI runs it via --include-ignored"]
async fn cs2_profile_scan() {
    use steamroids::session::{spawn_session, SessionConfig};
    use steamroids::{cs2, Error};

    // Prefer a dedicated CS2-licensed account; fall back to the plain account
    // (which usually lacks a CS2 license, so the GC won't welcome and we
    // soft-skip below).
    let Some(acc) = load_account("CS2").or_else(|| load_account("PLAIN")) else {
        skip(
            "cs2_profile_scan: set STEAM_TEST_CS2_ACCOUNT / _PASSWORD (a CS2-licensed account) or STEAM_TEST_PLAIN_*",
        );
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    // 1. Sign in (asserts a clean login) and bring up a live CM session.
    let Some(refresh) = sign_in_for_session("cs2", &acc, proxy.as_ref()).await else {
        return; // soft-skipped (email Guard / rate-limited)
    };
    let (handle, join) = spawn_session(SessionConfig {
        account_name: acc.username.clone(),
        refresh_token: refresh,
        proxy: proxy.clone(),
    })
    .await
    .expect("establish CM session");
    let steam_id = handle.steam_id();
    assert!(steam_id > 0, "expected a real SteamID");
    eprintln!("OK cs2: CM session up for steam_id {steam_id}");

    // 2. Attach the CS2 GC and wait for its welcome.
    let gc = cs2::attach(handle.clone()).await.expect("attach GC");
    if let Err(e) = gc.wait_ready(Duration::from_secs(25)).await {
        // Report the session state too: a `logged_off (eresult 6)` here means the
        // account is logged in elsewhere, not that it lacks a CS2 license.
        let reason = format!(
            "cs2_profile_scan: CS2 GC never became ready ({e}); session state = {:?}. \
             Causes: account lacks a CS2 license, or it's logged in elsewhere (eresult 6).",
            handle.state()
        );
        // logoff before skip: skip panics when live coverage is required
        handle.logoff().await.ok();
        let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
        skip(&reason);
        return;
    }
    eprintln!("OK cs2: GC welcomed us");

    // 3. Scan a profile and assert the round-trip. An explicit target (a known
    //    active player) guarantees data; otherwise scan our own account.
    let account_id = match env_opt("STEAM_TEST_CS2_TARGET_ID") {
        Some(id) => cs2::account_id_from_steam_id(
            id.parse()
                .expect("STEAM_TEST_CS2_TARGET_ID must be a SteamID64"),
        ),
        None => cs2::account_id_from_steam_id(steam_id),
    };
    let no_profile = match cs2::request_player_profile(&gc, account_id).await {
        Ok(profile) => {
            assert_eq!(
                profile.account_id, account_id,
                "GC returned a profile for the wrong account"
            );
            eprintln!(
                "OK cs2: profile account={} level={} xp={} rank={:?}",
                profile.account_id, profile.level, profile.current_xp, profile.competitive_rank
            );
            None
        }
        // The GC answered the handshake but had no profile row for us (e.g. a
        // brand-new account that has never played a match). The plumbing works;
        // there's just nothing to assert on.
        Err(Error::Network(msg)) => Some(format!(
            "cs2_profile_scan: GC returned no profile data ({msg})"
        )),
        Err(e) => panic!("cs2 profile request failed: {e}"),
    };

    // 4. Clean shutdown.
    handle.logoff().await.expect("clean logoff");
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("driver ends after logoff")
        .expect("driver did not panic")
        .expect("clean driver shutdown");

    // deferred so shutdown still runs when skip panics
    if let Some(reason) = no_profile {
        skip(&reason);
    }
}

/// CM server discovery against the public Steam directory (no credentials).
/// `#[ignore]`d so it stays out of the offline `test` job and only runs in the
/// live CI job; routed through the proxy for parity with the rest of the suite.
#[tokio::test]
#[ignore = "hits the public Steam directory; CI runs it via --include-ignored"]
async fn discover_cm_servers_lists_endpoints() {
    use steamroids::session::discover_cm_servers;

    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    let mut last_err = None;
    for attempt in 1..=4u32 {
        match discover_cm_servers(proxy.as_ref()).await {
            Ok(servers) => {
                assert!(!servers.is_empty(), "expected at least one CM server");
                let first = &servers[0];
                assert!(first.endpoint.contains(':'), "endpoint should be host:port");
                assert!(first.ws_url().starts_with("wss://"));
                eprintln!(
                    "OK discovery: {} CM servers, first = {}",
                    servers.len(),
                    first.ws_url()
                );
                return;
            }
            Err(Error::Network(msg)) if attempt < 4 => {
                eprintln!("discovery transient error (attempt {attempt}/4): {msg}");
                last_err = Some(msg);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => panic!("CM discovery failed: {e}"),
        }
    }
    panic!("CM discovery still failing: {last_err:?}");
}

/// Full CM logon: WebAPI sign-in (2FA account) -> refresh token -> CM
/// discovery -> connect over WSS -> `CMsgClientLogon`. The first end-to-end
/// "logged into Steam at the CM level" path. `#[ignore]`d (heavy, real login).
#[tokio::test]
#[ignore = "full CM logon against real Steam; CI runs it via --include-ignored"]
async fn cm_logon_over_wss() {
    use steamroids::session::{spawn_session, SessionConfig, SessionState};

    let Some(acc) = load_account("2FA") else {
        skip("cm_logon_over_wss: set STEAM_TEST_2FA_ACCOUNT / _PASSWORD / _SHARED_SECRET");
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    // 1. WebAPI sign-in for a fresh refresh token (with retry on proxy blips).
    let refresh = {
        let acc = &acc;
        let proxy = &proxy;
        let outcome = execute_with_retry("cm-signin", || {
            let mut s = SignIn::with_password(acc.username.clone(), acc.password.clone());
            if let Some(secret) = &acc.shared_secret {
                s = s.shared_secret(secret.clone());
            }
            if let Some(p) = proxy {
                s = s.proxy(p.clone());
            }
            s
        })
        .await;
        match outcome {
            SignInOutcome::Success { refresh_token, .. } => refresh_token,
            other => panic!("expected sign-in success, got {other:?}"),
        }
    };

    // Park the (valid) refresh token so the friends live test reuses it instead
    // of logging Account A in a second time — CM logon below doesn't consume it.
    persist_token(TOKEN_FILE_A, &refresh);

    // 2. Establish a self-healing session — discover -> connect -> logon all
    //    happen inside spawn_session — and run the background driver.
    let config = SessionConfig {
        account_name: acc.username.clone(),
        refresh_token: refresh,
        proxy: proxy.clone(),
    };
    let (handle, join) = spawn_session(config).await.expect("establish CM session");
    assert!(handle.steam_id() > 0, "expected a real SteamID");
    assert!(handle.state().is_ready(), "session should report LoggedOn");
    eprintln!(
        "OK CM session: steamid={} state={}",
        handle.steam_id(),
        handle.state().label()
    );

    // 3. Receive an unsolicited event, stay alive across heartbeat intervals,
    //    then shut down cleanly when all handles drop.
    let mut events = handle.subscribe();
    let first = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("expected an unsolicited event within 10s")
        .expect("event channel stayed open");
    eprintln!("OK driver: first event emsg={}", first.emsg);

    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(!join.is_finished(), "driver should still be running");

    // Clean logoff: tell Steam, tear down the socket, driver stops.
    drop(events);
    handle.logoff().await.expect("clean logoff");
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("driver task ends after logoff")
        .expect("driver task did not panic")
        .expect("clean driver shutdown");
    assert!(
        matches!(handle.state(), SessionState::LoggedOff { .. }),
        "state should be LoggedOff after logoff, was {}",
        handle.state().label()
    );
    eprintln!(
        "OK driver: clean logoff, final state={}",
        handle.state().label()
    );
}

/// Mint a web token over a live CM session and use it to fetch a page that
/// only renders for a signed-in account.
///
/// First unified `ServiceMethod` call this crate has ever sent, so the
/// offline tests can't show Steam actually accepts the frame. `#[ignore]`d
/// (real login).
///
/// Uses the plain account, not the 2FA one: `cm_logon_over_wss` already logs
/// the 2FA account in, and a second concurrent login would reuse the same
/// TOTP code inside its 30s window and get rejected. The plain account has no
/// shared secret to collide on.
#[tokio::test]
#[ignore = "live: needs STEAM_TEST_PLAIN_* and talks to real Steam"]
async fn web_session_authenticates_a_community_request() {
    use steamroids::session::{spawn_session, SessionConfig};

    let Some(acc) = load_account("PLAIN") else {
        skip("web_session: set STEAM_TEST_PLAIN_ACCOUNT / _PASSWORD");
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    let Some(refresh_token) = sign_in_for_session("web-session", &acc, proxy.as_ref()).await else {
        return;
    };

    let (handle, join) = spawn_session(SessionConfig {
        account_name: acc.username.clone(),
        refresh_token: refresh_token.clone(),
        proxy: proxy.clone(),
    })
    .await
    .expect("establish CM session");
    assert!(handle.steam_id() > 0, "expected a real SteamID");
    eprintln!(
        "OK web-session: CM session up for steam_id {}",
        handle.steam_id()
    );

    let web = steamroids::web::request_web_token(&handle, &refresh_token, proxy.as_ref())
        .await
        .expect("mint web token");
    eprintln!("OK web-session: minted web token for {}", web.steam_id());

    let body = web
        .get("https://steamcommunity.com/my/gcpd/730")
        .await
        .expect("gcpd fetch");

    // a signed-out fetch redirects to the login page instead
    assert!(
        !body.contains("g_steamID = false"),
        "GCPD returned a signed-out page, the cookie did not authenticate"
    );
    eprintln!("OK web-session: GCPD rendered signed-in");

    handle.logoff().await.expect("clean logoff");
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("driver task ends after logoff")
        .expect("driver task did not panic")
        .expect("clean driver shutdown");
}

/// Mint a web token over a live CM session and fetch this account's GCPD
/// competitive matchmaking cooldown through it.
///
/// The account under test may or may not have an active cooldown, so this
/// does not assert one exists; it asserts the fetch reached the real GCPD
/// page and parsed cleanly. That distinction matters here specifically: the
/// wrong URL (the `/me/` alias, unresolved by our client) returns the Steam
/// Community shell under HTTP 200 with no GCPD content, which `parse_cooldown`
/// reads as "no cooldown" -- the same `Ok(None)` a clean account produces. So
/// the raw HTML is checked for `Competitive Matches`, a left-nav tab label
/// present on every GCPD page regardless of cooldown state, before the page
/// is handed to the parser. `#[ignore]`d (real login).
///
/// Uses the plain account for the same reason
/// `web_session_authenticates_a_community_request` does: `cm_logon_over_wss`
/// already logs the 2FA account in, and a second concurrent login would reuse
/// the same TOTP code inside its 30s window and get rejected.
#[tokio::test]
#[ignore = "live: needs STEAM_TEST_PLAIN_* and talks to real Steam"]
async fn web_session_fetches_the_cs2_cooldown() {
    use steamroids::session::{spawn_session, SessionConfig};

    let Some(acc) = load_account("PLAIN") else {
        skip("gcpd_cooldown: set STEAM_TEST_PLAIN_ACCOUNT / _PASSWORD");
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    let Some(refresh_token) = sign_in_for_session("gcpd-cooldown", &acc, proxy.as_ref()).await
    else {
        return;
    };

    let (handle, join) = spawn_session(SessionConfig {
        account_name: acc.username.clone(),
        refresh_token: refresh_token.clone(),
        proxy: proxy.clone(),
    })
    .await
    .expect("establish CM session");
    assert!(handle.steam_id() > 0, "expected a real SteamID");
    eprintln!(
        "OK gcpd-cooldown: CM session up for steam_id {}",
        handle.steam_id()
    );

    let web = steamroids::web::request_web_token(&handle, &refresh_token, proxy.as_ref())
        .await
        .expect("mint web token");
    eprintln!("OK gcpd-cooldown: minted web token for {}", web.steam_id());

    let url = format!(
        "https://steamcommunity.com/profiles/{}/gcpd/730?tab=matchmaking",
        web.steam_id()
    );
    let html = web.get(&url).await.expect("gcpd fetch");

    // a wrong URL (e.g. the /me/ alias) returns the community shell with no
    // GCPD content under HTTP 200, which parses to Ok(None) same as a clean
    // account -- assert we actually reached GCPD before trusting the parse.
    assert!(
        html.contains("Competitive Matches"),
        "fetched page is not the GCPD page (missing the 'Competitive Matches' tab); \
         check the /profiles/<steamid64>/ URL form is being used"
    );
    eprintln!("OK gcpd-cooldown: confirmed the GCPD page rendered");

    match steamroids::gcpd::parse_cooldown(&html) {
        Ok(Some(cd)) => eprintln!(
            "OK gcpd-cooldown: cooldown until {} (level {:?}, acknowledged {})",
            cd.expires_at_raw, cd.level, cd.acknowledged
        ),
        Ok(None) => eprintln!("OK gcpd-cooldown: no active cooldown"),
        Err(e) => panic!("gcpd cooldown parse failed: {e}"),
    }

    handle.logoff().await.expect("clean logoff");
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("driver task ends after logoff")
        .expect("driver task did not panic")
        .expect("clean driver shutdown");
}

/// The refresh-token flow validates the token locally (no WebAPI call) and
/// hands it back for a CM logon. `SteamClient` tokens can't be redeemed over
/// the WebAPI, so this flow mints **no** access token — it only asserts the
/// token decodes to a real Steam ID and isn't reported expired. Offline-safe
/// (no network), so it's *not* ignored.
#[tokio::test]
async fn refresh_token_flow_validates_token() {
    let Some(token) = env_opt("STEAM_TEST_REFRESH_TOKEN") else {
        skip("refresh_token_flow: set STEAM_TEST_REFRESH_TOKEN to run");
        return;
    };

    let outcome = execute_with_retry("refresh", || {
        with_optional_proxy(SignIn::with_refresh_token(token.clone()))
    })
    .await;

    match outcome {
        SignInOutcome::Success {
            steam_id,
            access_token,
            ..
        } => {
            assert!(steam_id > 0, "expected a real SteamID");
            assert!(
                access_token.is_none(),
                "the refresh flow no longer mints a web access token (SteamClient \
                 tokens are redeemed over the CM, not the WebAPI)"
            );
            eprintln!("OK refresh-token flow validated steam_id {steam_id}");
        }
        SignInOutcome::TokenRejected => {
            panic!("STEAM_TEST_REFRESH_TOKEN is expired — rotate the secret");
        }
        other => panic!("unexpected outcome for refresh-token flow: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::note_skip;

    #[test]
    fn note_skip_is_quiet_when_live_coverage_is_optional() {
        note_skip(false, "no secrets configured");
    }

    #[test]
    #[should_panic(expected = "STEAM_LIVE_REQUIRED is set but the live test skipped")]
    fn note_skip_panics_when_live_coverage_is_required() {
        note_skip(true, "no secrets configured");
    }
}
