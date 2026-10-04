//! The OS credential store, via the `keyring` crate:
//!
//! - macOS: Keychain (Security framework)
//! - Linux / BSD: Secret Service over the D-Bus session bus (GNOME Keyring, KWallet, KeePassXC)
//! - Windows: Credential Manager
//!
//! Headless machines (servers, CI, containers) usually have no D-Bus session;
//! [`backend`] then reports why the store is unavailable and callers fall back
//! to a passphrase (`GENOME_KEY` or a prompt).

use std::sync::OnceLock;

use zeroize::Zeroizing;

use crate::crypto::Key;
use crate::error::{AppError, ErrorKind, Result};

pub const SERVICE: &str = "genome-cli";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    SecretService,
    CredentialManager,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Keychain => "macOS Keychain",
            Self::SecretService => "Secret Service (D-Bus)",
            Self::CredentialManager => "Windows Credential Manager",
        }
    }

    /// The store compiled in for this OS.
    fn native() -> std::result::Result<Self, String> {
        if cfg!(target_os = "macos") {
            Ok(Self::Keychain)
        } else if cfg!(windows) {
            Ok(Self::CredentialManager)
        } else if cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")) {
            Ok(Self::SecretService)
        } else {
            Err("no OS credential store is supported on this platform".into())
        }
    }
}

fn err(m: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Crypto, m)
}

/// Is a D-Bus session bus reachable at all? Checked before talking to the
/// Secret Service so headless machines fail fast instead of timing out.
fn dbus_session() -> std::result::Result<(), String> {
    let addr = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").filter(|v| !v.is_empty());
    let socket = std::env::var_os("XDG_RUNTIME_DIR").map(|d| std::path::Path::new(&d).join("bus"));
    if addr.is_some() || socket.is_some_and(|s| s.exists()) {
        Ok(())
    } else {
        Err("no D-Bus session bus (DBUS_SESSION_BUS_ADDRESS is unset; typical of SSH sessions, servers and CI)".into())
    }
}

fn probe() -> std::result::Result<Backend, String> {
    let b = Backend::native()?;
    if b == Backend::SecretService {
        dbus_session()?;
    }
    let entry = keyring::Entry::new(SERVICE, "probe").map_err(|e| format!("{}: {e}", b.name()))?;
    match entry.get_secret() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(b),
        Err(e) => Err(format!("{} unavailable: {e}", b.name())),
    }
}

/// The usable credential store, or why there is none (probed once per process).
pub fn backend() -> std::result::Result<Backend, String> {
    static B: OnceLock<std::result::Result<Backend, String>> = OnceLock::new();
    B.get_or_init(probe).clone()
}

fn entry(account: &str) -> Result<keyring::Entry> {
    let b = backend().map_err(|why| err(format!("no OS keyring: {why}")))?;
    keyring::Entry::new(SERVICE, account).map_err(|e| err(format!("{}: {e}", b.name())))
}

pub fn get(account: &str) -> Result<Option<Key>> {
    match entry(account)?.get_secret() {
        Ok(s) => Key::from_bytes(&Zeroizing::new(s)).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(err(format!("OS keyring: {e}"))),
    }
}

pub fn set(account: &str, key: &Key) -> Result<()> {
    entry(account)?.set_secret(key.bytes()).map_err(|e| err(format!("OS keyring: {e}")))?;
    // Read back: some backends accept writes they cannot persist.
    match get(account)? {
        Some(k) if k.bytes() == key.bytes() => Ok(()),
        _ => Err(err("OS keyring did not store the key")),
    }
}

/// Remove an entry. Returns whether one existed (never, without a store).
pub fn delete(account: &str) -> Result<bool> {
    if backend().is_err() {
        return Ok(false);
    }
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(err(format!("OS keyring: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through the real store. Runs only where one is reachable and
    /// `GENOME_TEST_KEYRING=1` (CI sets it), so local runs never touch a
    /// developer's keychain.
    #[test]
    fn round_trip_when_available() {
        if std::env::var_os("GENOME_TEST_KEYRING").is_none_or(|v| v != "1") {
            eprintln!("SKIPPED keystore round trip: set GENOME_TEST_KEYRING=1 to use the OS credential store");
            return;
        }
        let b = match backend() {
            Ok(b) => b,
            Err(why) => {
                eprintln!("SKIPPED keystore round trip: {why}");
                return;
            }
        };
        let account = format!("test:{}", std::process::id());
        let key = Key::random().unwrap();
        set(&account, &key).unwrap_or_else(|e| panic!("{}: {}", b.name(), e.message));
        assert_eq!(get(&account).unwrap().unwrap().bytes(), key.bytes());
        assert!(delete(&account).unwrap());
        assert!(get(&account).unwrap().is_none());
    }
}
