use std::path::PathBuf;

fn main() {
    compile_protos();
    inject_tapd_defaults();
    inject_gateway_default();
    tauri_build::build();
}

/// Inject the default OZark gateway onion URL at compile time from the
/// `OZARK_DEFAULT_GATEWAY_URL` env var (a repo secret in CI). When set, the Vault
/// screen auto-connects to it (custodial method) with no manual entry; the user can
/// still override it in-app. When unset, no default is baked and the user enters
/// their own gateway. The onion is not a secret (NIP-98 + per-user isolation are the
/// protection) but is kept out of source.
fn inject_gateway_default() {
    println!("cargo:rerun-if-env-changed=OZARK_DEFAULT_GATEWAY_URL");
    if let Ok(url) = std::env::var("OZARK_DEFAULT_GATEWAY_URL") {
        let url = url.trim();
        if !url.is_empty() {
            println!("cargo:rustc-env=OZARK_DEFAULT_GATEWAY_URL={url}");
        }
    }
}

/// Compile the tapd / Lightning protobuf definitions into Rust gRPC clients.
fn compile_protos() {
    let proto_dir = PathBuf::from("proto");
    let protos = [
        "proto/tapcommon.proto",
        "proto/taprootassets.proto",
        "proto/assetwallet.proto",
        "proto/mint.proto",
        "proto/universe.proto",
        "proto/lightning.proto",
        "proto/routerrpc/router.proto",
        "proto/rfqrpc/rfq.proto",
        "proto/priceoraclerpc/price_oracle.proto",
        "proto/tapchannel.proto",
    ];
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&protos, &[proto_dir])
        .expect("failed to compile tapd protos");
}

/// Inject the default tapd connection values at **compile time** so the
/// "Nœud par défaut" / auto-connect feature works without ever committing real
/// credentials to source control. `tapd_defaults.rs` reads these back through
/// `option_env!`.
///
/// Two sources are supported, in priority order:
///   1. Environment variables `OZARK_DEFAULT_TAPD_{HOST,CERT,MACAROON}`
///      (used by CI, fed from repository secrets).
///   2. A **gitignored** `tapd-defaults.json` at the repo root, used for local
///      builds (see `docs/tapd-umbrel-setup.md`). Keys: `host`, `cert_pem`,
///      `macaroon_hex`.
///
/// For each value the environment variable always wins; the JSON file only fills
/// in the gaps. This is what closes the long-standing gap where the docs claimed
/// `tapd-defaults.json` was embedded "via build.rs" but nothing actually read it.
fn inject_tapd_defaults() {
    // The three compile-time keys and their matching JSON fields.
    let mapping = [
        ("OZARK_DEFAULT_TAPD_HOST", "host"),
        ("OZARK_DEFAULT_TAPD_CERT", "cert_pem"),
        ("OZARK_DEFAULT_TAPD_MACAROON", "macaroon_hex"),
    ];

    // Re-run this build script when any of the env vars change.
    for (env_key, _) in mapping {
        println!("cargo:rerun-if-env-changed={env_key}");
    }

    // build.rs runs from `src-tauri/`; the documented file lives at the repo
    // root. Also accept a copy placed alongside the crate, just in case.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let candidates = [
        PathBuf::from(&manifest_dir).join("../tapd-defaults.json"),
        PathBuf::from(&manifest_dir).join("tapd-defaults.json"),
    ];
    for c in &candidates {
        // Re-run when the file is created, edited, or deleted.
        println!("cargo:rerun-if-changed={}", c.display());
    }

    // Read the first candidate that exists and parse it leniently.
    let json_text = candidates
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    let parsed: serde_json::Value = match &json_text {
        Some(text) => serde_json::from_str(text).unwrap_or_else(|e| {
            println!("cargo:warning=tapd-defaults.json is present but not valid JSON: {e}");
            serde_json::Value::Null
        }),
        None => serde_json::Value::Null,
    };

    let mut have_any_default = false;
    for (env_key, json_key) in mapping {
        // The environment variable (e.g. a CI secret) takes precedence; the JSON
        // file only fills in the gaps. We always re-emit `cargo:rustc-env` here
        // (rather than relying on `option_env!` reading the inherited env), so the
        // value is injected deterministically through the whole Gradle → cargo →
        // rustc chain and the multi-line PEM cert is normalized to the single-line
        // form `tapd_defaults.rs` expects.
        let raw = match std::env::var(env_key) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => parsed
                .get(json_key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };
        if raw.trim().is_empty() {
            continue;
        }
        have_any_default = true;

        // `cargo:rustc-env` directives are single-line. The PEM certificate is
        // multi-line, so encode newlines as the literal two-character sequence
        // "\n"; `tapd_defaults.rs` restores them at runtime for the cert via
        // `.replace("\\n", "\n")`. Host and macaroon never contain newlines.
        let encoded = if env_key == "OZARK_DEFAULT_TAPD_CERT" {
            raw.replace("\r\n", "\n")
                .replace('\r', "\n")
                .replace('\n', "\\n")
        } else {
            raw.trim().to_string()
        };

        println!("cargo:rustc-env={env_key}={encoded}");
    }

    if !have_any_default {
        // Non-fatal: the build still succeeds, but make the missing default node
        // obvious instead of failing silently at runtime after an 80s poll.
        println!(
            "cargo:warning=No default tapd node configured (no OZARK_DEFAULT_TAPD_* env vars and no tapd-defaults.json at the repo root). Taproot Assets will NOT auto-connect; users must add a node manually. See docs/tapd-umbrel-setup.md."
        );
    }
}
