//! Secrets (tokens, passwords) live in the OS keyring — Secret Service
//! (GNOME Keyring/KWallet) on Linux, Credential Manager on Windows — never in
//! the config file.
//!
//! Keyring calls are blocking (a D-Bus round trip on Linux), so callers must
//! run them via `tokio::task::spawn_blocking`, never on the render thread.

use keyring::Entry;

const SERVICE: &str = "isamedia";

pub const JELLYFIN_TOKEN: &str = "jellyfin-token";
pub const JELLYFIN_PASSWORD: &str = "jellyfin-password";
pub const SONARR_API_KEY: &str = "sonarr-api-key";

/// Read a secret; a missing entry and an unavailable keyring both come back
/// as `None` (the caller falls back to interactive login either way).
pub fn get(key: &str) -> Option<String> {
    let entry = Entry::new(SERVICE, key)
        .map_err(|err| tracing::warn!(%err, key, "failed to open keyring entry"))
        .ok()?;
    match entry.get_password() {
        Ok(value) => Some(value),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            tracing::warn!(%err, key, "failed to read from system keyring");
            None
        }
    }
}

pub fn set(key: &str, value: &str) -> Result<(), keyring::Error> {
    Entry::new(SERVICE, key)?.set_password(value)
}

/// Remove a secret; deleting something that isn't there is not an error.
pub fn delete(key: &str) -> Result<(), keyring::Error> {
    match Entry::new(SERVICE, key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err),
    }
}
