// Cargo build script: compiles proto/delta_txn.proto into the generated
// Rust types grpc::server::pb (see that module's own doc comment) re-runs
// automatically whenever the proto file changes (cargo tracks build.rs's
// own file-read dependencies for this). `build_client(false)`: this crate
// is a server only -- it never calls itself or another delta-txn-service
// instance as a gRPC client, so there's no reason to generate (and
// maintain the compile cost of) client stub code here. A downstream
// *consumer* wanting a Rust client would need its own tonic-prost-build
// step with `build_client(true)` against the same proto -- this repo
// doesn't provide one; KernelLake's own C++ client
// (github.com/<org>/kernel-lake's cmake/ThirdPartyDeltaTxnProto.cmake)
// generates its C++ equivalent independently, from a vendored copy of
// this same proto file.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("proto");
    let proto_file = proto_dir.join("delta_txn.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&[proto_file], &[proto_dir])?;

    Ok(())
}
