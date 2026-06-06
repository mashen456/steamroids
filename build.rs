//! Compiles vendored Steam `.proto` files into Rust via `prost-build`.
//!
//! See `protos/COMMIT.txt` for the upstream commit the vendored files match.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Files we actively compile. Imports they pull in (e.g. `enums.proto`,
    // `google/protobuf/descriptor.proto`) are resolved via the include paths
    // below but only the files listed here generate Rust types.
    let protos = [
        "protos/steam/steammessages_base.proto",
        "protos/steam/enums.proto",
        "protos/steam/enums_clientserver.proto",
        "protos/steam/steammessages_unified_base.steamclient.proto",
        "protos/steam/steammessages_auth.steamclient.proto",
        "protos/steam/steammessages_credentials.steamclient.proto",
        "protos/steam/steammessages_clientserver_login.proto",
        "protos/steam/encrypted_app_ticket.proto",
        "protos/steam/steammessages_clientserver.proto",
        "protos/steam/steammessages_clientserver_2.proto",
    ];

    // Game Coordinator (CS2) protos. These are package-less like the Steam set,
    // and several names collide (`CMsgProtoBufHeader`, `CMsgClientHello`,
    // `ECommunityItemClass`) — the GC ones are *different* messages with their
    // own field layout. Compiling them into the same flat module would clash, so
    // they get their own `OUT_DIR/gc` output, surfaced as `proto::gc`.
    let gc_protos = [
        "protos/csgo/steammessages.proto",
        "protos/csgo/engine_gcmessages.proto",
        "protos/csgo/gcsystemmsgs.proto",
        "protos/csgo/base_gcmessages.proto",
        "protos/csgo/gcsdk_gcmessages.proto",
        "protos/csgo/cstrike15_gcmessages.proto",
    ];

    for p in protos.iter().chain(gc_protos.iter()) {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=protos/google/protobuf/descriptor.proto");
    println!("cargo:rerun-if-changed=build.rs");

    // Use the bundled protoc so neither contributors nor CI need to install one.
    let protoc_binary = protoc_bin_vendored::protoc_bin_path()?;
    env::set_var("PROTOC", protoc_binary);

    let steam_dir = PathBuf::from("protos/steam");
    let csgo_dir = PathBuf::from("protos/csgo");
    let google_dir = PathBuf::from("protos");

    prost_build::Config::new().compile_protos(&protos, &[steam_dir, google_dir.clone()])?;

    // Separate module so the GC namespace can't collide with the Steam one.
    let gc_out = PathBuf::from(env::var("OUT_DIR")?).join("gc");
    std::fs::create_dir_all(&gc_out)?;
    let mut gc_config = prost_build::Config::new();
    gc_config.out_dir(&gc_out);
    gc_config.compile_protos(&gc_protos, &[csgo_dir, google_dir])?;

    Ok(())
}
