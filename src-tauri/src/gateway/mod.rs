//! OZark gateway client: reach a shared tapd node over its Tor onion with NIP-98
//! auth, instead of holding the tapd macaroon in the app. See `client` for the
//! transport and `commands` for the Tauri surface.

pub mod client;
pub mod commands;
