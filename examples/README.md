# Examples

Each example proves one capability of the library independently. They're
permanent — meant to stay passing as the crate evolves, doubling as smoke
tests.

There is no `06`: the CS2 Game Coordinator scan landed as `07_scan_one_profile`
and the numbering was never reused.

| # | Example | Proves | Network needed | Status |
| --- | --- | --- | --- | --- |
| 01 | [`01_totp.rs`](./01_totp.rs) | Steam TOTP algorithm produces correct shape | No | ✅ working |
| 02 | [`02_proxy_test.rs`](./02_proxy_test.rs) | SOCKS5 / HTTP-CONNECT relay works end-to-end | Yes (proxy + httpbin.org) | ✅ working |
| 03 | [`03_ws_echo.rs`](./03_ws_echo.rs) | WSS handshake works, optionally through a proxy | Yes (echo.websocket.events) | ✅ working |
| 04 | [`04_signin_credentials.rs`](./04_signin_credentials.rs) | `SignIn` builder for password + optional 2FA + optional proxy, end-to-end against `IAuthenticationService` | Yes (api.steampowered.com) | ✅ live |
| 05 | [`05_signin_refresh_token.rs`](./05_signin_refresh_token.rs) | `SignIn` builder for refresh-token reuse: the JWT is validated **locally** (`SteamID` + `exp`) and handed back, no `WebAPI` call and no access token | No | ✅ working |
| 07 | [`07_scan_one_profile.rs`](./07_scan_one_profile.rs) | Full stack: sign-in → CM session → CS2 GC attach + welcome → `PlayerProfile` (level, XP, rank) | Yes (Steam + CS2 GC) | ✅ live |
| 08 | [`08_profile_details.rs`](./08_profile_details.rs) | Keyless persona summary + public profile fields over the CM session | Yes (Steam) | ✅ live |
| 09 | [`09_friends.rs`](./09_friends.rs) | Friends list with resolved names, vanity-URL lookup, optional add / remove | Yes (Steam + steamcommunity.com) | ✅ live |
| 10 | [`10_account_dump.rs`](./10_account_dump.rs) | Everything fetchable for one account in one session: persona, profile, friends, avatar bytes, CS2 profile | Yes (Steam + avatar CDN) | ✅ live |
| 11 | [`11_persist_login.rs`](./11_persist_login.rs) | Bring-your-own-storage token persistence: password login once, then `spawn_session` from the stored refresh token | Yes (Steam) | ✅ live |

## Run

```bash
SHARED_SECRET=AAAAAAAAAAAAAAAAAAAAAAAAAAA= cargo run --example 01_totp
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 02_proxy_test
PROXY_URL="socks5://user:pass@host:1080"   cargo run --example 03_ws_echo

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  SHARED_SECRET=base64SharedSecret== \
  PROXY_URL="socks5://user:pass@host:1080" \
  cargo run --example 04_signin_credentials

REFRESH_TOKEN=eyJhbGc... \
  PROXY_URL="socks5://user:pass@host:1080" \
  cargo run --example 05_signin_refresh_token

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  TARGET_STEAMID=76561198000000000 \
  cargo run --example 07_scan_one_profile

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  cargo run --example 08_profile_details

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  RESOLVE_VANITY=gabelogannewell \
  cargo run --example 09_friends

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  cargo run --example 10_account_dump

STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
  SHARED_SECRET=base64SharedSecret== \
  TOKEN_FILE=refresh_token.txt \
  cargo run --example 11_persist_login
```

All env vars beyond the required credentials are optional: `PROXY_URL` works on
every networked example, `SHARED_SECRET` only matters for accounts with the
mobile authenticator, and `TARGET_STEAMID` defaults to the logged-in account.
Each example's module doc lists its own full set.
