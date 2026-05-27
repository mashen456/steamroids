# protos/

Vendored `.proto` files from upstream Steam sources.

**This directory is empty as of `0.0.1`** — we'll populate it for `0.1.0` once
the login flow is in scope.

## What goes here

The files we'll need, copied from SteamKit2's `Resources/SteamKit2/Protobufs/`:

| File | Source | Used for |
|---|---|---|
| `steammessages_base.proto` | SteamKit2 | EMsg envelope, common types |
| `steammessages_auth.steamclient.proto` | SteamKit2 | Login / refresh-token flow |
| `steammessages_clientserver_login.proto` | SteamKit2 | `CMsgClientLogon` |
| `gcsystemmsgs.proto` | SteamKit2 | GC envelope (`CMsgClientFromGC` / `…ToGC`) |
| `cstrike15_gcmessages.proto` | SteamKit2 | CS2 profile request / response |

## How to vendor

Upstream: <https://github.com/SteamRE/SteamKit/tree/master/Resources/SteamKit2/Protobufs>

```bash
# from this directory
curl -O https://raw.githubusercontent.com/SteamRE/SteamKit/master/Resources/SteamKit2/Protobufs/steamclient/steammessages_base.proto
# … repeat for each file above
```

When pulling in, record the upstream commit hash in this README so we can
diff against newer Valve revisions cleanly.

## Generation

`build.rs` (added in `0.1.0`) will run `prost-build` over this directory at
compile time. Generated Rust code goes into `OUT_DIR` and is included via
`include!` macros in the relevant module — no checked-in generated code.
