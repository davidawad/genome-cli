//! The OS credential store, via the `keyring` crate:
//!
//! - macOS: Keychain (Security framework)
//! - Linux / BSD: Secret Service over the D-Bus session bus (GNOME Keyring, KWallet, KeePassXC)
//! - Windows: Credential Manager
//!
//! Headless machines (servers, CI, containers) usually have no D-Bus session;
//! [`backend`] then reports why the store is unavailable and callers fall back
//! to a passphrase (`GENOME_KEY` or a prompt).
//!
//! The store is opt-in (`--kek keyring`, `db unlock`); key files are the
//! default. `GENOME_NO_KEYRING=1` forbids it outright. For tests,
//! `GENOME_TEST_KEYSTORE_DIR` replaces it with one file per entry in that
//! directory, so keyring flows run in CI without a real credential store.

use std::path::PathBuf;
use std::sync::OnceLock;

use zeroize::Zeroizing;

use crate::crypto::Key;
use crate::error::{AppError, ErrorKind, Result};

pub const SERVICE: &str = "genome-cli";
pub const NO_KEYRING_ENV: &str = "GENOME_NO_KEYRING";
const TEST_DIR_ENV: &str = "GENOME_TEST_KEYSTORE_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    SecretService,
    CredentialManager,
    TestDir,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Keychain => "macOS Keychain",
            Self::SecretService => "Secret Service (D-Bus)",
            Self::CredentialManager => "Windows Credential Manager",
            Self::TestDir => "test keystore directory (GENOME_TEST_KEYSTORE_DIR)",
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

fn forbidden() -> bool {
    std::env::var_os(NO_KEYRING_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

fn test_dir() -> Option<PathBuf> {
    std::env::var_os(TEST_DIR_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn test_file(account: &str) -> Option<PathBuf> {
    test_dir().map(|d| d.join(account.replace(':', "_")))
}

/// The usable credential store, or why there is none (probed once per process).
pub fn backend() -> std::result::Result<Backend, String> {
    if forbidden() {
        return Err(format!("the OS keyring is disabled ({NO_KEYRING_ENV} is set)"));
    }
    if test_dir().is_some() {
        return Ok(Backend::TestDir);
    }
    static B: OnceLock<std::result::Result<Backend, String>> = OnceLock::new();
    B.get_or_init(probe).clone()
}

fn entry(account: &str) -> Result<keyring::Entry> {
    let b = backend().map_err(|why| err(format!("no OS keyring: {why}")))?;
    keyring::Entry::new(SERVICE, account).map_err(|e| err(format!("{}: {e}", b.name())))
}

pub fn get(account: &str) -> Result<Option<Key>> {
    backend().map_err(|why| err(format!("no OS keyring: {why}")))?;
    if let Some(f) = test_file(account) {
        return match std::fs::read(&f) {
            Ok(b) => Key::from_bytes(&Zeroizing::new(b)).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(err(format!("{}: {e}", f.display()))),
        };
    }
    match entry(account)?.get_secret() {
        Ok(s) => Key::from_bytes(&Zeroizing::new(s)).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(err(format!("OS keyring: {e}"))),
    }
}

pub fn set(account: &str, key: &Key) -> Result<()> {
    backend().map_err(|why| err(format!("no OS keyring: {why}")))?;
    if let Some(f) = test_file(account) {
        std::fs::create_dir_all(f.parent().expect("dir"))?;
        return std::fs::write(&f, key.bytes()).map_err(|e| err(format!("{}: {e}", f.display())));
    }
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
    if let Some(f) = test_file(account) {
        return Ok(std::fs::remove_file(f).is_ok());
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
