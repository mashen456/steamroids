//! Live integration tests that authenticate against real Steam.
//!
//! These hit `api.steampowered.com` with real credentials, so they are
//! **opt-in** and driven by environment variables — loaded locally from a
//! git-ignored `.env` (via `dotenvy`) or, in CI, from encrypted secrets. When
//! the variables for a test are absent it prints a `SKIP` notice and returns
//! instead of failing, so the normal `cargo test` run, contributor machines,
//! and fork PRs stay green without credentials.
//!
//! # Accounts under test
//!
//! - **2FA account** (`STEAM_TEST_2FA_*`) — password plus a mobile
//!   authenticator shared secret.
//! - **Plain account** (`STEAM_TEST_PLAIN_*`) — password only. For a green
//!   `login OK` this account must have Steam Guard fully disabled; if it still
//!   has email Guard the test soft-skips (our flow can't enter an email code
//!   yet).
//! - **Refresh token** (`STEAM_TEST_REFRESH_TOKEN`, optional) — exercises the
//!   token-reuse path.
//!
//! The real-login tests are `#[ignore]`d so a stray `cargo test` never fires
//! live logins; CI opts in with `-- --include-ignored`. Each account is logged
//! in at most once per run (the 2FA login happens only inside `cm_logon_over_wss`).
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
//! - `STEAM_TEST_REFRESH_TOKEN` (optional)
//! - `STEAM_TEST_PROXY_URL` (optional) — routes every call through a proxy

use std::time::Duration;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::Error;

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
    for attempt in 1..=MAX_ATTEMPTS {
        match build().execute().await {
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

/// Drive the password flow for `acc` and assert a clean login, treating
/// transient / unsupported states as loud skips rather than failures.
async fn run_password_login(label: &str, acc: Account) {
    let outcome = execute_with_retry(label, || {
        let mut signin = SignIn::with_password(acc.username.clone(), acc.password.clone());
        if let Some(secret) = &acc.shared_secret {
            signin = signin.shared_secret(secret.clone());
        }
        with_optional_proxy(signin)
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
        }
        SignInOutcome::NeedsMobileGuardCode => {
            panic!(
                "[{label}] account needs mobile 2FA — set STEAM_TEST_{}_SHARED_SECRET",
                label.to_uppercase()
            );
        }
        SignInOutcome::NeedsEmailGuardCode { email_domain } => {
            eprintln!("SKIP [{label}]: account still has email Steam Guard (domain {email_domain}); disable Steam Guard for a real login test");
        }
        SignInOutcome::RateLimited { retry_hint } => {
            eprintln!("SKIP [{label}]: Steam rate-limited (retry {retry_hint:?})");
        }
        SignInOutcome::InvalidCredentials => {
            panic!("[{label}] username/password rejected by Steam");
        }
        other => panic!("[{label}] unexpected outcome: {other:?}"),
    }
}

// NB: the 2FA account's password login is exercised as step 1 of
// `cm_logon_over_wss`; there is no separate `login_account_with_2fa` test, so
// that account is never logged in twice in one run (concurrent logins reuse the
// same TOTP code within a 30s window and Steam rejects the duplicate).

/// Password only, no 2FA. `#[ignore]`d so it only runs on explicit opt-in.
#[tokio::test]
#[ignore = "full password login; CI runs it via --include-ignored"]
async fn login_account_without_2fa() {
    let Some(acc) = load_account("PLAIN") else {
        eprintln!("SKIP login_account_without_2fa: set STEAM_TEST_PLAIN_ACCOUNT / _PASSWORD");
        return;
    };
    run_password_login("plain", acc).await;
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
    use steamroids::session::{discover_cm_servers, CmConnection};

    let Some(acc) = load_account("2FA") else {
        eprintln!(
            "SKIP cm_logon_over_wss: set STEAM_TEST_2FA_ACCOUNT / _PASSWORD / _SHARED_SECRET"
        );
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

    // 2. Discover CM servers.
    let servers = discover_cm_servers(proxy.as_ref())
        .await
        .expect("CM discovery failed");

    // 3. Connect + logon, trying a few servers in case one is unreachable.
    let mut last_err = None;
    for server in servers.iter().take(5) {
        match CmConnection::connect(&server.ws_url(), proxy.as_ref()).await {
            Ok(mut conn) => match conn.logon(&acc.username, refresh.expose()).await {
                Ok(logged) => {
                    assert!(logged.steam_id > 0, "expected a real SteamID");
                    assert!(logged.session_id != 0, "expected a session id");
                    eprintln!(
                        "OK CM logon via {}: steamid={} session={} heartbeat={:?}",
                        server.ws_url(),
                        logged.steam_id,
                        logged.session_id,
                        logged.heartbeat_interval
                    );

                    // 4. Heartbeat: keep the session alive past several
                    //    intervals. Surviving the timeout = success.
                    let mut msgs = 0u32;
                    let run = conn.run(logged.heartbeat_interval, |_m| msgs += 1);
                    match tokio::time::timeout(Duration::from_secs(25), run).await {
                        Err(_elapsed) => {
                            eprintln!("OK heartbeat: session alive 25s, {msgs} msgs received");
                        }
                        Ok(Ok(())) => panic!("session logged off during heartbeat window"),
                        Ok(Err(e)) => panic!("heartbeat loop failed (session dropped): {e}"),
                    }
                    return;
                }
                Err(e) => panic!("CM logon rejected: {e}"),
            },
            Err(e) => {
                eprintln!("connect to {} failed, trying next: {e}", server.ws_url());
                last_err = Some(e);
            }
        }
    }
    panic!("could not connect to any CM server: {last_err:?}");
}

/// The refresh-token round-trip: a single `GenerateAccessTokenForApp` call.
/// Cheap and low-risk, so this one is *not* ignored.
#[tokio::test]
async fn refresh_token_flow_issues_access_token() {
    let Some(token) = env_opt("STEAM_TEST_REFRESH_TOKEN") else {
        eprintln!("SKIP refresh_token_flow: set STEAM_TEST_REFRESH_TOKEN to run");
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
                access_token.is_some(),
                "refresh flow should mint an access token"
            );
            eprintln!("OK refresh-token flow for steam_id {steam_id}");
        }
        SignInOutcome::RateLimited { retry_hint } => {
            eprintln!("SKIP: Steam rate-limited the refresh-token test (retry {retry_hint:?})");
        }
        SignInOutcome::TokenRejected => {
            panic!("STEAM_TEST_REFRESH_TOKEN was rejected (expired/revoked) — rotate the secret");
        }
        other => panic!("unexpected outcome for refresh-token flow: {other:?}"),
    }
}
