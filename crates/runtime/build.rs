//! Generates the internal wire types (F020) from `proto/` at build
//! time. `protox` compiles the .proto sources to a descriptor set in
//! pure Rust, so CI needs no system `protoc`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest.join("../../proto");
    let file = proto_root.join("fishpond/internal/v1/identity.proto");

    let descriptor = protox::compile([file], [&proto_root])?;
    prost_build::Config::new().compile_fds(descriptor)?;

    println!("cargo:rerun-if-changed={}", proto_root.display());
    Ok(())
}
