fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Kept inside the crate (not at the workspace root) so the `.proto` sources
    // ship in the published `.crate` tarball and this build script still works
    // for downstream consumers.
    let proto_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto_file = proto_root.join("infrastore").join("v1").join("store.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[proto_file.to_str().unwrap()],
            &[proto_root.to_str().unwrap()],
        )?;
    Ok(())
}
