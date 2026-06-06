//! Generated protobuf types from the vendored Steam `.proto` files.
//!
//! The schemas live under `protos/`; `build.rs` runs `prost-build` over them
//! at compile time and drops the generated Rust into `$OUT_DIR`. The Steam
//! protos carry no `package` declaration, so prost emits everything into one
//! file (`_.rs`) re-exported flat from this module.
//!
//! See `protos/COMMIT.txt` for the upstream pin.

// Generated code is not held to our pedantic lint bar.
#![allow(
    clippy::pedantic,
    clippy::all,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unreachable_pub
)]

include!(concat!(env!("OUT_DIR"), "/_.rs"));

/// Game Coordinator protobuf types (CS2 / `csgo`).
///
/// These are vendored package-less like the Steam protos, but several message
/// names (`CMsgProtoBufHeader`, `CMsgClientHello`, …) collide with the Steam set
/// while being entirely different messages — the GC `CMsgProtoBufHeader` has its
/// own field layout. They therefore compile into their own module here rather
/// than the flat root above.
pub mod gc {
    #![allow(
        clippy::pedantic,
        clippy::all,
        missing_docs,
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        unreachable_pub
    )]

    include!(concat!(env!("OUT_DIR"), "/gc/_.rs"));
}
