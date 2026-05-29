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
    ];

    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=protos/google/protobuf/descriptor.proto");
    println!("cargo:rerun-if-changed=build.rs");

    // Use the bundled protoc so neither contributors nor CI need to install one.
    let protoc_binary = protoc_bin_vendored::protoc_bin_path()?;
    env::set_var("PROTOC", protoc_binary);

    let steam_dir = PathBuf::from("protos/steam");
    let google_dir = PathBuf::from("protos");

    prost_build::Config::new().compile_protos(&protos, &[steam_dir, google_dir])?;

    Ok(())
}
