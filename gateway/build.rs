use std::path::PathBuf;

/// Compile the tapd / litd protobuf subset the gateway needs: `taprpc`
/// (list_assets / decode_addr), `universerpc` (stats / roots), `mintrpc`
/// (mint), plus the Lightning-asset stack — `tapchannelrpc` + `rfqrpc` and
/// their lnd deps (`lnrpc`, `routerrpc`, `priceoraclerpc`). Requires `protoc`
/// on PATH (installed in CI). Mirrors the wallet app's proto build.
fn main() {
    let proto_dir = PathBuf::from("proto");
    let protos = [
        "proto/tapcommon.proto",
        "proto/taprootassets.proto",
        "proto/universe.proto",
        "proto/mint.proto",
        "proto/lightning.proto",
        "proto/routerrpc/router.proto",
        "proto/rfqrpc/rfq.proto",
        "proto/priceoraclerpc/price_oracle.proto",
        "proto/tapchannel.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&protos, &[proto_dir])
        .expect("failed to compile tapd protos");
}
