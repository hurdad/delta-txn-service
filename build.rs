fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile(&["proto/delta_txn.proto"], &["proto"])?;

    Ok(())
}
