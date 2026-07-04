use std::path::PathBuf;

/// Compile the minimal tapd protobuf subset the gateway needs for its read
/// endpoints: `taprpc` (list_assets / decode_addr) and `universerpc`
/// (stats / roots), plus their shared `tapcommon` types. Requires `protoc` on
/// PATH (installed in CI). Mirrors the wallet app's proto build.
fn main() {
    let proto_dir = PathBuf::from("proto");
    let protos = [
        "proto/tapcommon.proto",
        "proto/taprootassets.proto",
        "proto/universe.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&protos, &[proto_dir])
        .expect("failed to compile tapd protos");
}
