// Cargo build script: compiles proto/delta_txn.proto into the generated
// Rust types grpc::server::pb (see that module's own doc comment) re-runs
// automatically whenever the proto file changes (cargo tracks build.rs's
// own file-read dependencies for this).
//
// `build_client(true)`: this binary itself never calls another
// delta-txn-service instance as a gRPC client, but the integration test
// suite under tests/ does -- it drives a real DeltaTxnServiceServer over
// a real gRPC connection (see tests/common/mod.rs), and a hand-written
// client would just be reimplementing what tonic-prost-build already
// generates correctly. The generated client is `pub` from grpc::server::pb
// like everything else here, so any downstream Rust consumer of this
// crate as a library can also use it directly rather than needing its own
// tonic-prost-build step against a vendored copy of the proto; a non-Rust
// client (e.g. a C++ consumer) still generates its own stubs independently
// either way.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("proto");
    let proto_file = proto_dir.join("delta_txn.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
