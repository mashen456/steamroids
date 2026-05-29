# Examples

Each example proves one capability of the library independently. They're
permanent — meant to stay passing as the crate evolves, doubling as smoke
tests.

| # | Example | Proves | Network needed | Status |
| --- | --- | --- | --- | --- |
| 01 | [`01_totp.rs`](./01_totp.rs) | Steam TOTP algorithm produces correct shape | No | ✅ working |
| 02 | [`02_proxy_test.rs`](./02_proxy_test.rs) | SOCKS5 / HTTP-CONNECT relay works end-to-end | Yes (proxy + httpbin.org) | ✅ working |
| 03 | [`03_ws_echo.rs`](./03_ws_echo.rs) | WSS handshake works, optionally through a proxy | Yes (echo.websocket.events) | ✅ working |
| 04 | [`04_signin_credentials.rs`](./04_signin_credentials.rs) | `SignIn` builder API for password + optional 2FA + optional proxy | (will need: api.steampowered.com + CM) | 🚧 skeleton — terminates at `Error::NotImplemented` until 0.1.x auth flow lands |
| 05 | [`05_signin_refresh_token.rs`](./05_signin_refresh_token.rs) | `SignIn` builder API for refresh-token reuse + optional proxy | (will need: CM) | 🚧 skeleton — same status as 04 |

## Run

```bash
# Already working:
SHARED_SECRET=AAAAAAAAAAAAAAAAAAAAAAAAAAA= cargo run --example 01_totp
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 02_proxy_test
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 03_ws_echo

# API-skeleton demos (terminate cleanly at `Error::NotImplemented`):
STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  SHARED_SECRET=base64SharedSecret== \
  PROXY_URL="socks5://user:pass@host:1080" \
  cargo run --example 04_signin_credentials

REFRESH_TOKEN=eyJhbGc... \
  PROXY_URL="socks5://user:pass@host:1080" \
  cargo run --example 05_signin_refresh_token
```

All env vars on examples 04/05 are independent — proxy and 2FA secret are
optional, refresh token / credentials are required for their respective flow.

## Coming up

- `06_scan_one_profile.rs` — CS2 GC profile request (needs `0.2.0`)
