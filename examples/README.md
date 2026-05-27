# Examples

Each example proves one capability of the library independently. They're
permanent — meant to stay passing as the crate evolves, doubling as smoke
tests.

| # | Example | Proves | Network needed |
|---|---|---|---|
| 01 | [`01_totp.rs`](./01_totp.rs) | Steam TOTP algorithm produces correct shape | No |
| 02 | [`02_proxy_test.rs`](./02_proxy_test.rs) | SOCKS5 / HTTP-CONNECT relay works end-to-end | Yes (proxy + httpbin.org) |
| 03 | [`03_ws_echo.rs`](./03_ws_echo.rs) | WSS handshake works, optionally through a proxy | Yes (echo.websocket.events) |

## Run all

```bash
SHARED_SECRET=AAAAAAAAAAAAAAAAAAAAAAAAAAA= cargo run --example 01_totp
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 02_proxy_test
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 03_ws_echo
```

## Coming up

- `04_login_password.rs` — full login with password + 2FA (needs `0.1.0`)
- `05_login_refresh.rs` — login with stored refresh token (needs `0.1.0`)
- `06_scan_one_profile.rs` — CS2 GC profile request (needs `0.2.0`)
