//! Compiles the BIP300/301 enforcer's `ValidatorService` protobuf definitions.
//!
//! Uses `protox` (a pure-Rust `protoc` implementation) so the build doesn't depend on a
//! system-installed `protoc`, mirroring `thunder-rust`'s `lib/build.rs`.
//!
//! Reads these from the `bip300301_enforcer` git submodule (see `../.gitmodules`) vendored at
//! `bip300301_enforcer/` -- pinned to a specific, reviewed commit, the same way `thunder-rust`
//! (a reference BIP300/301 sidechain implementation) vendors the same repo for the same reason.
//! Run `git submodule update --init` (or clone this repo with `--recurse-submodules`) if it's
//! missing.

use std::{env, fs, path::PathBuf};

use protox::prost::Message as _;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const COMMON_PROTO: &str = "bip300301_enforcer/proto/cusf/common/v1/common.proto";
    const VALIDATOR_PROTO: &str = "bip300301_enforcer/proto/cusf/mainchain/v1/validator.proto";
    const WALLET_PROTO: &str = "bip300301_enforcer/proto/cusf/mainchain/v1/wallet.proto";
    const PROTOS: &[&str] = &[COMMON_PROTO, VALIDATOR_PROTO, WALLET_PROTO];
    const INCLUDES: &[&str] = &["bip300301_enforcer/proto"];

    println!("cargo:rerun-if-changed={COMMON_PROTO}");
    println!("cargo:rerun-if-changed={VALIDATOR_PROTO}");
    println!("cargo:rerun-if-changed={WALLET_PROTO}");

    let file_descriptor_set = protox::compile(PROTOS, INCLUDES)?;
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR environment variable not set"));
    let file_descriptor_path = out_dir.join("file_descriptor_set.bin");
    fs::write(&file_descriptor_path, file_descriptor_set.encode_to_vec())?;

    tonic_prost_build::configure()
        .skip_protoc_run()
        .build_server(false)
        .file_descriptor_set_path(&file_descriptor_path)
        .compile_with_config(prost_build::Config::new(), PROTOS, INCLUDES)?;

    Ok(())
}
