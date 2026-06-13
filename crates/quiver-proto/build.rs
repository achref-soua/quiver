// SPDX-License-Identifier: AGPL-3.0-only
//! Generate the gRPC client/server code from `proto/quiver.proto`.
//!
//! `protoc` is supplied by `protoc-bin-vendored`, so a clean clone builds with
//! no system protobuf compiler installed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: a build script runs single-threaded before any threads are
    // spawned, so setting a process environment variable here is sound.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    println!("cargo:rerun-if-changed=proto/quiver.proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/quiver.proto"], &["proto"])?;
    Ok(())
}
