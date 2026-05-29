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

Minimal subset needed for `0.1.x` (login + heartbeat).

| File | Used for |
|---|---|
| `steam/steammessages_base.proto` | `CMsgProtoBufHeader` (header on every EMsg) |
| `steam/enums.proto` | `EResult`, `EUniverse`, etc. |
| `steam/enums_clientserver.proto` | the `EMsg` enum |
| `steam/steammessages_unified_base.steamclient.proto` | base for service-style messages |
| `steam/steammessages_auth.steamclient.proto` | `BeginAuthSessionViaCredentials`, `Poll`, `GenerateAccessTokenForApp` |
| `steam/steammessages_credentials.steamclient.proto` | credential exchange types |
| `steam/steammessages_clientserver_login.proto` | `CMsgClientLogon`, `CMsgClientLogonResponse`, `CMsgClientHeartBeat` |
| `google/protobuf/descriptor.proto` | resolved via include path so `extend google.protobuf.MessageOptions` parses; not compiled (prost ships `prost-types` for it) |

For `0.2.x` (CS2 Game Coordinator) we'll add at least `csgo/gcsystemmsgs.proto`
and `csgo/cstrike15_gcmessages.proto`.

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
