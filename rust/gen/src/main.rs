//! Generates the Rust gRPC code for the Sessions API.
//!
//!   cargo run --manifest-path rust/gen/Cargo.toml -- PROTO_DIR OUT_DIR
//!
//! Needs `protoc` on PATH (or $PROTOC). The output is committed, so users of the crate don't need it.
use std::{env, path::PathBuf};

fn main() {
    let mut args = env::args().skip(1);
    let (proto_dir, out_dir) = match (args.next(), args.next()) {
        (Some(p), Some(o)) => (PathBuf::from(p), PathBuf::from(o)),
        _ => {
            eprintln!("usage: chainguard-sandbox-gen PROTO_DIR OUT_DIR");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&out_dir).expect("creating the output directory");
    let proto = proto_dir.join("sandbox/runtime/session/v1/session.proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true) // the tests run an in-process fake server
        .out_dir(&out_dir)
        .compile_protos(&[proto], &[proto_dir])
        .expect("compiling the protos");
}
