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
