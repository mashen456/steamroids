# Unified ServiceMethod Support and Web Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a logged-on CM session call Steam unified service methods, and use that to mint a web access token so callers can authenticate to `steamcommunity.com` (GCPD, store, community) without a second login.

**Architecture:** Steam's unified services ride the existing CM WebSocket as `k_EMsgServiceMethodCallFromClient` (151) frames whose protobuf header carries `target_job_name = "Interface.Method#Version"`. Responses arrive as `k_EMsgServiceMethodResponse` (147) with `jobid_target` set to our `jobid_source`, which the driver's existing `dispatch` already correlates, so no new correlation machinery is needed. On top of that we add one call, `Authentication.GenerateAccessTokenForApp#1`, which exchanges the `SteamClient`-platform refresh token for a web access token. The `steamLoginSecure` cookie is then `urlencode("<steamid64>||<access_token>")`.

**Tech Stack:** Rust 1.86 (edition 2021), tokio, prost, `url` (already a dependency, for percent-encoding).

## Background: why this design

Verified before writing this plan, so implementers do not re-litigate it:

- `GenerateAccessTokenForApp` **cannot** be called over the plain WebAPI with a `SteamClient`-platform refresh token. Steam answers `AccessDenied` (eresult 15). This was tried and reverted in commit `5eaf2c6`; the removed code sent `refresh_token`, `steamid` and `renewal_type` correctly, so the failure was not a malformed request. The exchange must ride the authenticated CM session. See the note at `src/auth/signin.rs:419-424`.
- `DoctorMcKay/node-steam-session` confirms the mechanism. For `SteamClient` and `MobileApp` platforms, `getWebCookies()` calls `refreshAccessToken()`, which calls `generateAccessTokenForApp()` through its transport, then builds:
  ```js
  let cookieValue = encodeURIComponent([this.steamID.getSteamID64(), this.accessToken].join('||'));
  return [`steamLoginSecure=${cookieValue}`, `sessionid=${sessionId}`];
  ```
  (`WebBrowser`-platform tokens take a different path via `login.steampowered.com/jwt/finalizelogin`. We never issue those, so ignore it.)
- All required protobufs are **already vendored and compiled**. `build.rs:17` compiles `protos/steam/steammessages_auth.steamclient.proto`, which defines `CAuthentication_AccessToken_GenerateForApp_Request` (line 225) and `_Response` (line 231).
- EMsg values are **verified from the vendored enum**, `protos/steam/enums_clientserver.proto:35-41`. Do not take EMsg numbers from memory or from the web. This codebase shipped `TryAnotherCM = 42` (it is 48) precisely because someone hardcoded an unverifiable number.

## Global Constraints

- Rust edition 2021, `rust-version` 1.86, pinned by `rust-toolchain.toml`.
- `#![forbid(unsafe_code)]`; `clippy::all` and `clippy::pedantic` are `warn`; `missing_docs` is `warn`. CI runs `cargo clippy --all-features --all-targets -- -D warnings`, so any warning fails the build.
- **Add no new dependencies.** Everything needed is present. Use `url::form_urlencoded` for percent-encoding.
- Comments are CAVEMAN-MINIMAL: terse lowercase fragments, no prose, no articles, no rationale. Example: `// unified call rides emsg 151`. Rustdoc on public items is the exception and should be proper prose matching the existing voice.
- **No em-dashes anywhere**, in code, comments, rustdoc, commit messages or this plan's output.
- Minimal diff. Do not reformat unrelated code or add stray comments.
- TDD: write the failing test, watch it fail, implement minimally, watch it pass, commit. One conventional commit per task.
- Run `cargo fmt` before each commit.

---

### Task 1: Carry `target_job_name` through the send path

Unified calls are ordinary frames whose header names the target method. The driver builds every header in one place, so this is a plumbing change plus a new EMsg constant.

**Files:**
- Modify: `src/session/driver.rs` (the `send_frame` fn, the `Command::Request` variant, and its handler in `run_connected`)
- Test: `src/session/driver.rs` (the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub(crate) const EMSG_SERVICE_METHOD_CALL_FROM_CLIENT: u32 = 151;`
  - `pub(crate) const EMSG_SERVICE_METHOD_RESPONSE: u32 = 147;`
  - `send_frame(write, steam_id, session_id, emsg, jobid_source, job_name: Option<&str>, body) -> Result<()>`
  - `Command::Request { emsg, body, job_name: Option<String>, deadline, reply }`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/session/driver.rs`. This asserts the encoded frame's header carries the job name, decoding it back with the crate's own codec.

```rust
#[test]
fn service_method_frames_carry_the_target_job_name() {
    let header = CMsgProtoBufHeader {
        steamid: Some(7),
        client_sessionid: Some(1),
        jobid_source: Some(42),
        target_job_name: Some("Authentication.GenerateAccessTokenForApp#1".to_string()),
        ..Default::default()
    };
    let frame = codec::encode_raw(EMSG_SERVICE_METHOD_CALL_FROM_CLIENT, &header, &[]);
    let decoded = codec::try_decode(&frame).unwrap().expect("proto frame");

    assert_eq!(decoded.emsg, EMSG_SERVICE_METHOD_CALL_FROM_CLIENT);
    assert_eq!(
        decoded.header.target_job_name.as_deref(),
        Some("Authentication.GenerateAccessTokenForApp#1")
    );
    assert_eq!(decoded.header.jobid_source, Some(42));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib service_method_frames_carry_the_target_job_name`

Expected: FAIL to compile, `cannot find value EMSG_SERVICE_METHOD_CALL_FROM_CLIENT in this scope`.

- [ ] **Step 3: Add the EMsg constants**

Place these next to the other EMsg constants near the top of `src/session/driver.rs`. Values are from `protos/steam/enums_clientserver.proto:35-41`; keep that citation in the comment so the next reader can re-verify.

```rust
// enums_clientserver.proto: k_EMsgServiceMethodCallFromClient
pub(crate) const EMSG_SERVICE_METHOD_CALL_FROM_CLIENT: u32 = 151;
// enums_clientserver.proto: k_EMsgServiceMethodResponse
pub(crate) const EMSG_SERVICE_METHOD_RESPONSE: u32 = 147;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib service_method_frames_carry_the_target_job_name`

Expected: PASS.

- [ ] **Step 5: Thread `job_name` through `send_frame`**

Change the signature and header construction. This takes `send_frame` to 7 parameters, which is at but not over clippy's `too_many_arguments` threshold, so it will not warn.

```rust
async fn send_frame(
    write: &mut SplitSink<SteamWebSocket, WsMessage>,
    steam_id: u64,
    session_id: i32,
    emsg: u32,
    jobid_source: Option<u64>,
    job_name: Option<&str>,
    body: &[u8],
) -> Result<()> {
    let header = CMsgProtoBufHeader {
        steamid: Some(steam_id),
        client_sessionid: Some(session_id),
        jobid_source,
        target_job_name: job_name.map(ToString::to_string),
        ..Default::default()
    };
    write_frame(write, codec::encode_raw(emsg, &header, body)).await
}
```

- [ ] **Step 6: Update every `send_frame` call site**

Run `cargo build 2>&1` and fix each error by inserting `None` for the new parameter. Call sites are the `Notify`, `Logoff`, `Request` and heartbeat arms in `run_connected`, plus any in `answer_while_down`. Do not change their behaviour.

- [ ] **Step 7: Add `job_name` to `Command::Request` and to `dispatch_request`**

In the `Command` enum:

```rust
Request {
    emsg: u32,
    body: Vec<u8>,
    job_name: Option<String>,
    deadline: Instant,
    reply: oneshot::Sender<Result<SteamMessage>>,
},
```

In the `Command::Request` arm of `run_connected`, destructure `job_name` and pass `job_name.as_deref()` to `send_frame`. Fix the `answer_while_down` arm and any test constructors the compiler flags.

`Command::Request` is built by the private helper `dispatch_request` at `src/session/driver.rs:258`. Note it takes the message **by reference** and encodes internally, it does not take bytes. Add the parameter there too:

```rust
async fn dispatch_request<Req, Resp>(
    &self,
    emsg: u32,
    req: &Req,
    job_name: Option<String>,
    timeout: Duration,
    check_eresult: bool,
) -> Result<Resp>
where
    Req: prost::Message,
    Resp: prost::Message + Default,
```

and set `job_name` on the `Command::Request` it constructs. Its two existing callers, `request_with_timeout` (line 220) and `request_ignoring_eresult`, both pass `None`.

- [ ] **Step 8: Run the full suite**

Run: `cargo build --all-targets && cargo test --all-features`

Expected: all existing tests still pass. If `tests/session_handlers.rs` constructs `Command::Request`, add the new field there.

- [ ] **Step 9: Commit**

```bash
cargo fmt
git add src/session/driver.rs tests/
git commit -m "feat(session): carry target_job_name on outgoing frames

Unified service methods ride an ordinary CM frame whose header names the
target method. Adds the ServiceMethod EMsg constants (verified against
protos/steam/enums_clientserver.proto) and threads an optional job name
through Command::Request and send_frame. No behaviour change for existing
callers, which pass None."
```

---

### Task 2: `SessionHandle::call_service`

The public entry point. Correlation needs no new code: unified responses set `jobid_target` to our `jobid_source`, and `dispatch` at `src/session/driver.rs` already resolves pending requests on that field.

**Files:**
- Modify: `src/session/driver.rs` (impl block for `SessionHandle`)
- Test: `src/session/driver.rs` tests module

**Interfaces:**
- Consumes: `EMSG_SERVICE_METHOD_CALL_FROM_CLIENT`, `Command::Request { job_name, .. }` from Task 1.
- Produces:
  ```rust
  pub async fn call_service<Req, Resp>(
      &self,
      interface: &str,
      method: &str,
      version: u32,
      req: &Req,
  ) -> Result<Resp>
  where
      Req: prost::Message,
      Resp: prost::Message + Default;
  ```

- [ ] **Step 1: Write the failing test**

Uses the `for_test` seam added during the audit. It drives a fake driver: assert the command the handle emits, then answer it.

```rust
#[tokio::test]
async fn call_service_sends_a_named_job_and_decodes_the_reply() {
    let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(77);

    let task = tokio::spawn(async move {
        handle
            .call_service::<_, CMsgProtoBufHeader>(
                "Authentication",
                "GenerateAccessTokenForApp",
                1,
                &CMsgProtoBufHeader::default(),
            )
            .await
    });

    let Some(Command::Request { emsg, job_name, reply, .. }) = commands.recv().await else {
        panic!("expected a Request command");
    };
    assert_eq!(emsg, EMSG_SERVICE_METHOD_CALL_FROM_CLIENT);
    assert_eq!(
        job_name.as_deref(),
        Some("Authentication.GenerateAccessTokenForApp#1")
    );

    // answer with a body that decodes as the response type
    let body = CMsgProtoBufHeader { steamid: Some(99), ..Default::default() }.encode_to_vec();
    reply
        .send(Ok(SteamMessage {
            emsg: EMSG_SERVICE_METHOD_RESPONSE,
            header: CMsgProtoBufHeader::default(),
            body,
        }))
        .expect("send reply");

    let resp = task.await.expect("task").expect("call_service");
    assert_eq!(resp.steamid, Some(99));
}
```

Note: `CMsgProtoBufHeader` is used as a stand-in message so this test depends on no other task. Task 3 uses the real types.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib call_service_sends_a_named_job_and_decodes_the_reply`

Expected: FAIL to compile, `no method named call_service found`.

- [ ] **Step 3: Implement `call_service`**

Add to the `impl SessionHandle` block, next to `request`. Reuse whatever private helper `request_with_timeout` already funnels through so the deadline, sweep and header-eresult checks all apply unchanged. If that helper does not currently take a job name, add the parameter and pass `None` from the existing callers.

```rust
/// Call a Steam unified service method over this session.
///
/// Unified services are addressed by name rather than by `EMsg`: the frame
/// goes out as `ServiceMethodCallFromClient` with a header naming
/// `Interface.Method#Version`. The reply is correlated by job id exactly like
/// [`Self::request`], so the usual deadline applies.
///
/// ```no_run
/// # async fn demo(session: &steamroids::session::SessionHandle) -> steamroids::Result<()> {
/// # use steamroids::proto::{
/// #     CAuthenticationAccessTokenGenerateForAppRequest,
/// #     CAuthenticationAccessTokenGenerateForAppResponse,
/// # };
/// let req = CAuthenticationAccessTokenGenerateForAppRequest::default();
/// let resp: CAuthenticationAccessTokenGenerateForAppResponse = session
///     .call_service("Authentication", "GenerateAccessTokenForApp", 1, &req)
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// [`Error::Remote`] if the reply header carries a non-OK `EResult`,
/// [`Error::Timeout`] if no reply arrives before the deadline,
/// [`Error::WebSocket`] if the session stopped, or [`Error::Codec`] if the
/// body does not decode as `Resp`.
pub async fn call_service<Req, Resp>(
    &self,
    interface: &str,
    method: &str,
    version: u32,
    req: &Req,
) -> Result<Resp>
where
    Req: prost::Message,
    Resp: prost::Message + Default,
{
    let job_name = format!("{interface}.{method}#{version}");
    self.dispatch_request(
        EMSG_SERVICE_METHOD_CALL_FROM_CLIENT,
        req,
        Some(job_name),
        DEFAULT_REQUEST_TIMEOUT,
        true,
    )
    .await
}
```

Note `dispatch_request` takes `req` by reference and encodes it internally, so do **not** call `encode_to_vec` here. `check_eresult` stays `true`: unified replies put their result in the header, which is exactly what that check reads.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib call_service_sends_a_named_job_and_decodes_the_reply`

Expected: PASS.

- [ ] **Step 5: Run the full suite and lints**

Run: `cargo test --all-features && cargo clippy --all-targets --all-features -- -D warnings && cargo test --doc`

Expected: all green. The doctest above is `no_run`, so it must compile but will not connect.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/session/driver.rs
git commit -m "feat(session): add SessionHandle::call_service for unified services

Steam's unified services are addressed by name, not EMsg. call_service
sends ServiceMethodCallFromClient with target_job_name set to
Interface.Method#Version. Replies correlate by job id through the existing
dispatch path, so deadlines and header-eresult checks apply unchanged."
```

---

### Task 3: Mint a web access token

**Files:**
- Create: `src/web.rs`
- Modify: `src/lib.rs` (add `pub mod web;` and a bullet in the crate docs module list)
- Test: `src/web.rs` tests module

**Interfaces:**
- Consumes: `SessionHandle::call_service` from Task 2; `SessionHandle::steam_id()` (already exists); `crate::auth::RefreshToken` (already exists).
- Produces: `pub async fn request_web_token(session: &SessionHandle, refresh_token: &RefreshToken) -> Result<WebSession>` and `pub struct WebSession` (fields defined in Task 4).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn request_web_token_calls_generate_access_token_for_app() {
    let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(76_561_198_000_000_001);

    let task = tokio::spawn(async move {
        request_web_token(&handle, &RefreshToken::new("stored-refresh-token".to_string())).await
    });

    let Some(Command::Request { job_name, body, reply, .. }) = commands.recv().await else {
        panic!("expected a Request command");
    };
    assert_eq!(
        job_name.as_deref(),
        Some("Authentication.GenerateAccessTokenForApp#1")
    );

    let sent = CAuthenticationAccessTokenGenerateForAppRequest::decode(body.as_slice())
        .expect("decode request");
    assert_eq!(sent.refresh_token.as_deref(), Some("stored-refresh-token"));
    assert_eq!(sent.steamid, Some(76_561_198_000_000_001));
    // k_ETokenRenewalType_None: do not rotate the refresh token
    assert_eq!(sent.renewal_type, Some(0));

    let resp = CAuthenticationAccessTokenGenerateForAppResponse {
        access_token: Some("minted-access-token".to_string()),
        refresh_token: None,
    };
    reply
        .send(Ok(SteamMessage {
            emsg: EMSG_SERVICE_METHOD_RESPONSE,
            header: CMsgProtoBufHeader::default(),
            body: resp.encode_to_vec(),
        }))
        .expect("send reply");

    let web = task.await.expect("task").expect("request_web_token");
    assert_eq!(web.access_token(), "minted-access-token");
    assert_eq!(web.steam_id(), 76_561_198_000_000_001);
}

#[tokio::test]
async fn request_web_token_errors_when_steam_returns_no_token() {
    let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(5);

    let task = tokio::spawn(async move {
        request_web_token(&handle, &RefreshToken::new("t".to_string())).await
    });

    let Some(Command::Request { reply, .. }) = commands.recv().await else {
        panic!("expected a Request command");
    };
    let resp = CAuthenticationAccessTokenGenerateForAppResponse {
        access_token: None,
        refresh_token: None,
    };
    reply
        .send(Ok(SteamMessage {
            emsg: EMSG_SERVICE_METHOD_RESPONSE,
            header: CMsgProtoBufHeader::default(),
            body: resp.encode_to_vec(),
        }))
        .expect("send reply");

    let err = task.await.expect("task").unwrap_err();
    assert!(matches!(err, Error::Remote(_)), "{err:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::`

Expected: FAIL, module `web` does not exist.

- [ ] **Step 3: Create `src/web.rs` with the module doc and the call**

```rust
//! Steam web session support.
//!
//! A logged-on CM session can mint a web access token for the same account,
//! which authenticates requests to `steamcommunity.com` and the store without
//! a second login. This is what the Steam client itself does so its embedded
//! browser is signed in.
//!
//! The exchange must ride the CM session: Steam refuses
//! `GenerateAccessTokenForApp` over the plain `WebAPI` for the
//! `SteamClient`-platform refresh tokens this crate issues, answering
//! `AccessDenied`. See [`crate::auth`] for the platform split.

use prost::Message;

use crate::auth::RefreshToken;
use crate::proto::{
    CAuthenticationAccessTokenGenerateForAppRequest,
    CAuthenticationAccessTokenGenerateForAppResponse,
};
use crate::session::SessionHandle;
use crate::{Error, Result};

// k_ETokenRenewalType_None: leave the refresh token as it is
const TOKEN_RENEWAL_NONE: i32 = 0;

/// Exchange this session's refresh token for a web access token.
///
/// `refresh_token` must be the token this session logged on with. The returned
/// [`WebSession`] carries the cookie needed to authenticate web requests as
/// this account.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the exchange or returns no token, plus
/// any transport error from the underlying session call.
pub async fn request_web_token(
    session: &SessionHandle,
    refresh_token: &RefreshToken,
) -> Result<WebSession> {
    let req = CAuthenticationAccessTokenGenerateForAppRequest {
        refresh_token: Some(refresh_token.expose().to_string()),
        steamid: Some(session.steam_id()),
        renewal_type: Some(TOKEN_RENEWAL_NONE),
    };
    let resp: CAuthenticationAccessTokenGenerateForAppResponse = session
        .call_service("Authentication", "GenerateAccessTokenForApp", 1, &req)
        .await?;

    let access_token = resp.access_token.filter(|t| !t.is_empty()).ok_or_else(|| {
        Error::Remote("GenerateAccessTokenForApp returned no access token".into())
    })?;

    Ok(WebSession {
        steam_id: session.steam_id(),
        access_token,
    })
}
```

Leave `WebSession` itself for Task 4; for now add a minimal definition so this compiles:

```rust
/// An authenticated Steam web session.
#[derive(Clone)]
pub struct WebSession {
    steam_id: u64,
    access_token: String,
}

impl WebSession {
    /// The account this session authenticates as.
    pub fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// The minted web access token.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod web;` alongside the other module declarations, and add a bullet to the crate-level module list matching the existing style.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib web::`

Expected: both PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/web.rs src/lib.rs
git commit -m "feat(web): mint a web access token over the CM session

request_web_token exchanges the session's SteamClient refresh token for a
web access token via Authentication.GenerateAccessTokenForApp#1. Steam
refuses this exchange over the plain WebAPI for SteamClient-platform
tokens, so it rides the authenticated CM session."
```

---

### Task 4: Build the `steamLoginSecure` cookie

**Files:**
- Modify: `src/web.rs`
- Test: `src/web.rs` tests module

**Interfaces:**
- Consumes: `WebSession` from Task 3.
- Produces: `WebSession::cookie_header(&self) -> String` and `WebSession::with_session_id(self, session_id: impl Into<String>) -> Self`.

- [ ] **Step 1: Write the failing test**

The critical assertion is that `||` percent-encodes to `%7C%7C`. A raw pipe in a cookie value is what breaks this most often.

```rust
#[test]
fn cookie_header_encodes_the_pipe_separator() {
    let web = WebSession {
        steam_id: 76_561_198_000_000_001,
        access_token: "eyJhbGci.eyJzdWIi.sig-part_x".to_string(),
        session_id: None,
    };
    assert_eq!(
        web.cookie_header(),
        "steamLoginSecure=76561198000000001%7C%7CeyJhbGci.eyJzdWIi.sig-part_x"
    );
}

#[test]
fn cookie_header_appends_a_session_id_when_set() {
    let web = WebSession {
        steam_id: 1,
        access_token: "tok".to_string(),
        session_id: None,
    }
    .with_session_id("abc123");
    assert_eq!(
        web.cookie_header(),
        "steamLoginSecure=1%7C%7Ctok; sessionid=abc123"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::cookie`

Expected: FAIL to compile, no field `session_id` and no method `cookie_header`.

- [ ] **Step 3: Add the field and the methods**

Add `session_id: Option<String>` to `WebSession`, and set `session_id: None` in the constructor inside `request_web_token`. Then:

```rust
impl WebSession {
    /// Attach a `sessionid` cookie.
    ///
    /// Only needed for state-changing POSTs, which Steam guards with a CSRF
    /// token that must match this cookie. Plain GETs (profile pages, GCPD)
    /// authenticate on `steamLoginSecure` alone. The value is caller-supplied
    /// so this crate takes no random-number dependency; any opaque string
    /// works as long as the same value goes in the form field.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// The value for a `Cookie:` request header, authenticating as this account.
    ///
    /// ```
    /// # use steamroids::web::WebSession;
    /// # fn demo(web: WebSession) {
    /// let client = reqwest::Client::new();
    /// let request = client
    ///     .get("https://steamcommunity.com/my/gcpd/730")
    ///     .header(reqwest::header::COOKIE, web.cookie_header());
    /// # let _ = request;
    /// # }
    /// ```
    #[must_use]
    pub fn cookie_header(&self) -> String {
        // steamLoginSecure is "<steamid64>||<access token>", url-encoded
        let raw = format!("{}||{}", self.steam_id, self.access_token);
        let encoded: String = url::form_urlencoded::byte_serialize(raw.as_bytes()).collect();
        match &self.session_id {
            Some(sid) => format!("steamLoginSecure={encoded}; sessionid={sid}"),
            None => format!("steamLoginSecure={encoded}"),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib web::`

Expected: all PASS. If `byte_serialize` encodes a character the assertion did not expect, fix the **test's** expected string only after confirming by hand what Steam accepts. Do not weaken the `%7C%7C` assertion.

- [ ] **Step 5: Add a redacting Debug impl**

`WebSession` holds a credential. Deriving `Debug` would print it into logs. Match how the crate redacts elsewhere (see `ProxyConfig`'s Debug impl in `src/transport/proxy.rs`).

```rust
impl std::fmt::Debug for WebSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSession")
            .field("steam_id", &self.steam_id)
            .field("access_token", &"<redacted>")
            .field("session_id", &self.session_id)
            .finish()
    }
}
```

Add a test asserting the token does not appear:

```rust
#[test]
fn debug_does_not_leak_the_access_token() {
    let web = WebSession {
        steam_id: 1,
        access_token: "super-secret".to_string(),
        session_id: None,
    };
    assert!(!format!("{web:?}").contains("super-secret"));
}
```

- [ ] **Step 6: Run everything**

Run: `cargo test --all-features && cargo clippy --all-targets --all-features -- -D warnings && cargo test --doc && cargo fmt --check`

Expected: all green.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/web.rs
git commit -m "feat(web): build the steamLoginSecure cookie header

steamLoginSecure is url-encoded \"<steamid64>||<access token>\", so the pipe
separator has to survive as %7C%7C. sessionid is caller-supplied and only
needed for state-changing POSTs, which keeps a random-number dependency out
of the crate. Debug redacts the token."
```

---

### Task 5: Prove it against real Steam

Everything above is offline. Nothing so far shows Steam actually accepts the frame, which is the whole risk: this is the first unified call the crate has ever sent.

**Files:**
- Modify: `tests/live_auth.rs`
- Modify: `.env.example` (document nothing new, but confirm the existing 2FA account vars cover this test)

**Interfaces:**
- Consumes: `request_web_token`, `WebSession::cookie_header` from Tasks 3 and 4.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the live test**

Follow the existing conventions in `tests/live_auth.rs` exactly: `#[ignore]`, the `skip()` helper so a missing secret skips locally but panics under `STEAM_LIVE_REQUIRED`, and `--nocapture`-friendly `println!` progress.

```rust
/// Mint a web token over a live CM session and use it to fetch a page that
/// only renders for a signed-in account.
#[tokio::test]
#[ignore = "live: needs STEAM_TEST_2FA_* and talks to real Steam"]
async fn web_session_authenticates_a_community_request() {
    use steamroids::session::{spawn_session, SessionConfig};

    let Some(acc) = load_account("2FA") else {
        skip("web_session: set STEAM_TEST_2FA_ACCOUNT / _PASSWORD / _SHARED_SECRET");
        return;
    };
    let proxy = env_opt("STEAM_TEST_PROXY_URL")
        .map(|u| ProxyConfig::parse(&u).expect("STEAM_TEST_PROXY_URL is not a valid proxy URL"));

    let Some(refresh_token) = sign_in_for_session("web-session", &acc, proxy.as_ref()).await else {
        return;
    };

    let (handle, driver) = spawn_session(SessionConfig::new(&acc.username, refresh_token.clone()))
        .await
        .expect("spawn session");

    let web = steamroids::web::request_web_token(&handle, &refresh_token)
        .await
        .expect("mint web token");
    println!("minted web token for {}", web.steam_id());

    let body = reqwest::Client::new()
        .get("https://steamcommunity.com/my/gcpd/730")
        .header(reqwest::header::COOKIE, web.cookie_header())
        .send()
        .await
        .expect("gcpd request")
        .text()
        .await
        .expect("gcpd body");

    // a signed-out fetch redirects to the login page instead
    assert!(
        !body.contains("g_steamID = false"),
        "GCPD returned a signed-out page, the cookie did not authenticate"
    );

    handle.logoff().await.expect("logoff");
    driver.await.expect("driver task").expect("driver result");
}
```

Every helper used above already exists in `tests/live_auth.rs`: `load_account(prefix)` at line 151 (returns `Account { username, password, shared_secret }`), `sign_in_for_session(label, &Account, Option<&ProxyConfig>)` at line 191 (returns `Option<RefreshToken>` and handles the retry-on-proxy-blip logic), `env_opt` at line 96, and `skip` at line 124. `sign_in_for_session` returning `None` means it already called the skip helper, so just return. Do not add new helpers.

Confirm `SessionConfig::new`'s real signature before writing the spawn line; `cm_logon_over_wss` at line 389 is the working reference for the whole spawn-and-logoff shape.

- [ ] **Step 2: Verify it compiles**

Run: `cargo test --test live_auth --no-run`

Expected: compiles clean. `reqwest` is already a dependency of the crate, so it is available to integration tests.

- [ ] **Step 3: Run it against Steam**

Run: `cargo test --test live_auth web_session_authenticates_a_community_request -- --include-ignored --nocapture`

Expected: PASS with the minted-token line printed.

**If it fails, this is the interesting part, so diagnose rather than retry:**
- Header eresult 15 (`AccessDenied`) means the unified call reached Steam but was refused. Check `target_job_name` is exactly `Authentication.GenerateAccessTokenForApp#1` and that `steamid` matches the logged-on account.
- A timeout means no reply correlated. Confirm the response arrived as EMsg 147 with `jobid_target` set, by subscribing to the session's event stream and logging what came back.
- A signed-out page with a successful mint means the cookie is malformed. Log `cookie_header()` and check the `%7C%7C` encoding survived.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add tests/live_auth.rs
git commit -m "test(live): prove the web session authenticates a real request

First unified ServiceMethod call this crate has sent, so the offline tests
cannot show Steam accepts the frame. Mints a token over a live CM session
and asserts GCPD renders signed-in rather than redirecting to login."
```

---

## Follow-on work, deliberately out of scope

Keep these out of this plan so it stays one testable subsystem. Each is worth its own plan once this lands:

1. **GCPD parsing.** With the cookie working, `steamcommunity.com/my/gcpd/730` becomes readable. This is HTML scraping, not protobuf, so it needs its own error handling and will break when Valve changes the page. Note the constraint found while scoping: GCPD is personal data behind your own login, so it covers accounts you control, not arbitrary players.
2. **Cheap CS2 wins that need none of this.** `CMsgGCCStrike15_v2_MatchmakingGC2ClientHello` already carries `vac_banned` (field 6), `penalty_seconds` (4) and `penalty_reason` (5), and `cs2::request_player_profile` already decodes that exact message and throws them away. Surfacing them on `PlayerProfile` is a struct extension with no new protocol work, and it works for arbitrary accounts. Do this first if bans and cooldowns are the actual goal.
3. **Revocation detection.** `SignIn::with_refresh_token().execute()` validates offline and so cannot tell "expired" from "revoked", which is why `execute_with_store` never falls back to the password flow for a revoked token. A CM-side token exchange makes revocation observable, because Steam actually answers. `request_web_token` is exactly that exchange.
4. **Prime status.** Still has no clean answer. There is no Prime field in the vendored protos; `prime_only` is a matchmaking search filter, not account state. The routes are the SharedObject cache for your own account, or inferring from a Premier rating, which gives false negatives.
