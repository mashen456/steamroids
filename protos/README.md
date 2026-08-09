# protos/

Vendored `.proto` files from upstream Steam sources. Compiled by
[`build.rs`](../build.rs) via [`prost-build`](https://crates.io/crates/prost-build);
generated Rust lands in `$OUT_DIR` and is re-exported from
[`steamroids::proto`](../src/proto.rs).

## Upstream

- Source: <https://github.com/SteamTracking/Protobufs>
- Pinned commit + vendor date: see [`COMMIT.txt`](COMMIT.txt)

`SteamTracking/Protobufs` is the upstream used by SteamKit (where it's pulled
in as a git submodule under `Resources/Protobufs`). We vendor flat copies so
the build has no submodule or network dependency.

## Currently vendored

18 files: 11 Steam, 6 CS2 Game Coordinator, and `descriptor.proto` for imports.
`build.rs` compiles the first 17; `descriptor.proto` is resolved via the include
path only.

### `steam/`: Connection Manager

Compiled into the flat [`proto`](../src/proto.rs) module.

| File | Used for |
|---|---|
| `steammessages_base.proto` | `CMsgProtoBufHeader` (header on every EMsg), `CMsgMulti` |
| `enums.proto` | `EResult`, `EUniverse`, etc. |
| `enums_clientserver.proto` | the `EMsg` enum |
| `steammessages_unified_base.steamclient.proto` | base for service-style messages |
| `steammessages_auth.steamclient.proto` | `BeginAuthSessionViaCredentials`, `PollAuthSessionStatus`, `GenerateAccessTokenForApp` |
| `steammessages_credentials.steamclient.proto` | credential exchange types |
| `steammessages_clientserver_login.proto` | `CMsgClientLogon`, `CMsgClientLogonResponse`, `CMsgClientHeartBeat`, `CMsgClientLoggedOff` |
| `steammessages_clientserver.proto` | `CMsgClientGamesPlayed` (launch an app so its GC routes to us) |
| `steammessages_clientserver_2.proto` | the `CMsgGCClient` client↔GC relay envelope |
| `steammessages_clientserver_friends.proto` | friends, nicknames, groups, chat, `CMsgClientPersonaState`, `CMsgClientFriendProfileInfo` |
| `encrypted_app_ticket.proto` | `EncryptedAppTicket`; imported by `steammessages_clientserver.proto` |

### `csgo/`: CS2 Game Coordinator

Compiled into a separate [`proto::gc`](../src/proto.rs) module (its own
`OUT_DIR/gc` output). These files are package-less like the Steam set and
several names collide (`CMsgProtoBufHeader`, `CMsgClientHello`,
`ECommunityItemClass`) while being *different* messages, so a shared flat
namespace would clash.

| File | Used for |
|---|---|
| `steammessages.proto` | the GC's own `CMsgProtoBufHeader` (its own field layout, not the CM one) |
| `gcsystemmsgs.proto` | `EGCBaseClientMsg`: `k_EMsgGCClientHello` / `…Welcome` / `…ConnectionStatus` |
| `gcsdk_gcmessages.proto` | `CMsgClientHello` / `CMsgClientWelcome`, `GCConnectionStatus` |
| `cstrike15_gcmessages.proto` | `ECsgoGCMsg`, `CMsgGCCStrike15_v2_ClientRequestPlayersProfile` / `…PlayersProfile` |
| `base_gcmessages.proto` | GC base types imported by the CS2 messages |
| `engine_gcmessages.proto` | `CEngineGotvSyncPacket`; imported by the CS2 messages |

### `google/`

| File | Used for |
|---|---|
| `google/protobuf/descriptor.proto` | resolved via include path so `extend google.protobuf.MessageOptions` parses; not compiled (prost ships `prost-types` for it) |

## Re-vendoring upstream changes

```bash
# Run from the repo root.
BASE='https://raw.githubusercontent.com/SteamTracking/Protobufs/<NEW_SHA>'
for f in \
  steam/steammessages_base.proto \
  steam/enums.proto \
  steam/enums_clientserver.proto \
  steam/steammessages_unified_base.steamclient.proto \
  steam/steammessages_auth.steamclient.proto \
  steam/steammessages_credentials.steamclient.proto \
  steam/steammessages_clientserver_login.proto \
  steam/steammessages_clientserver.proto \
  steam/steammessages_clientserver_2.proto \
  steam/steammessages_clientserver_friends.proto \
  steam/encrypted_app_ticket.proto \
  csgo/steammessages.proto \
  csgo/gcsystemmsgs.proto \
  csgo/gcsdk_gcmessages.proto \
  csgo/cstrike15_gcmessages.proto \
  csgo/base_gcmessages.proto \
  csgo/engine_gcmessages.proto \
  google/protobuf/descriptor.proto
do
  curl -sSf -o "protos/$f" "$BASE/$f"
done
```

Then update `protos/COMMIT.txt` and re-run `cargo build`. If new transitive
imports appear, `protoc` will fail with a clear "file not found" — fetch the
missing files and add them above.

## Conventions

- **No generated code committed.** Everything in this directory is source-of-truth
  upstream; `build.rs` does the codegen at compile time.
- **No edits to vendored files.** If we need to massage something, do it in
  `build.rs` (prost-build config) or in a wrapper module — never in the `.proto`.
- **Pin a specific commit**, never `master`. Steam protos change; we want
  intentional bumps.
