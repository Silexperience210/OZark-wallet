/// Default tapd connection values, injected at **compile time** from either
/// `tapd-defaults.json` (read by `build.rs`) or environment variables so that
/// real node credentials are never committed to source control.
///
/// **Priority:** `build.rs` sets `cargo:rustc-env=…` when `tapd-defaults.json`
/// exists next to `Cargo.toml`. If the file is absent, fall back to
/// pre-existing environment variables (e.g. CI secrets).
///
/// Set these when building the release APK to make the "Nœud par défaut" button
/// work out of the box:
///   - `OZARK_DEFAULT_TAPD_HOST`     — e.g. `https://<onion>:10029`
///   - `OZARK_DEFAULT_TAPD_CERT`     — the PEM-encoded TLS certificate
///   - `OZARK_DEFAULT_TAPD_MACAROON` — the hex-encoded macaroon
///
/// When unset, the defaults are empty and the user must configure their own tapd
/// node from the UI. This removes the previously hard-coded admin macaroon/cert/
/// onion address from the binary's source (they are no longer reverse-engineerable
/// from the repository). NOTE: any credentials that were committed in earlier
/// releases should be rotated, as they remain in git history.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TapdDefaults {
    pub host: String,
    pub cert_pem: String,
    pub macaroon_hex: String,
}

impl TapdDefaults {
    pub fn load() -> Self {
        Self {
            host: option_env!("OZARK_DEFAULT_TAPD_HOST")
                .unwrap_or("")
                .to_string(),
            cert_pem: option_env!("OZARK_DEFAULT_TAPD_CERT")
                .unwrap_or("")
                // Allow the cert to carry literal "\n" escapes in the env var.
                .replace("\\n", "\n"),
            // SECURITY: never bake a tapd macaroon into the (public, decompilable)
            // APK — an admin credential in the binary lets anyone act on the node.
            // The default node is therefore disabled (`is_empty()` below) until an
            // authenticated gateway holds the macaroon server-side. Advanced users
            // connect their own node from the UI.
            macaroon_hex: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.host.trim().is_empty() || self.macaroon_hex.trim().is_empty()
    }
}
