//! SSH keys as database keys: the DEK is encrypted to the user's SSH public
//! key with age (ssh-ed25519 and ssh-rsa recipients) and decrypted with the
//! matching private key. Pure Rust, so the same OpenSSH key files work on
//! macOS, Linux and Windows.
//!
//! Discovery: `GENOME_SSH_KEY` (a private key path), else `id_ed25519` then
//! `id_rsa` in `GENOME_SSH_DIR` or `~/.ssh` (`%USERPROFILE%\.ssh` on Windows).
//! A passphrase-protected key is unlocked with `GENOME_SSH_PASSPHRASE` or an
//! interactive prompt.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use age::secrecy::SecretString;
use ssh_key::{HashAlg, PrivateKey, PublicKey};
use zeroize::Zeroizing;

use crate::crypto::Key;
use crate::error::{AppError, ErrorKind, Result};
use crate::prompt::Prompter;

pub const SSH_KEY_ENV: &str = "GENOME_SSH_KEY";
pub const SSH_DIR_ENV: &str = "GENOME_SSH_DIR";
pub const SSH_PASS_ENV: &str = "GENOME_SSH_PASSPHRASE";
const CANDIDATES: [&str; 2] = ["id_ed25519", "id_rsa"];

fn err(m: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Crypto, m)
}

/// A usable SSH key pair.
#[derive(Debug, Clone)]
pub struct SshKey {
    pub identity: PathBuf,
    /// OpenSSH public key line (`ssh-ed25519 AAAA… comment`).
    pub public: String,
    /// `SHA256:…`, as `ssh-keygen -lf` prints it.
    pub fingerprint: String,
    pub algorithm: String,
    pub encrypted: bool,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn ssh_dir() -> PathBuf {
    env_path(SSH_DIR_ENV).unwrap_or_else(|| crate::platform::dirs::home().join(".ssh"))
}

/// Private keys to try, in order.
pub fn candidates() -> Vec<PathBuf> {
    match env_path(SSH_KEY_ENV) {
        Some(p) => vec![p],
        None => CANDIDATES.iter().map(|n| ssh_dir().join(n)).collect(),
    }
}

/// The first candidate that is a usable key, with why the others were not.
pub fn discover() -> (Option<SshKey>, Vec<String>) {
    let mut notes = Vec::new();
    for p in candidates().into_iter().filter(|p| p.exists()) {
        match load(&p) {
            Ok(k) => return (Some(k), notes),
            Err(e) => notes.push(format!("{}: {}", p.display(), e.message)),
        }
    }
    (None, notes)
}

/// Parse a public key line and return it normalized with its fingerprint.
pub fn describe_public(line: &str) -> Result<(String, String, String)> {
    let pk = PublicKey::from_openssh(line.trim()).map_err(|e| err(format!("not an OpenSSH public key: {e}")))?;
    let public = pk.to_openssh().map_err(|e| err(e.to_string()))?;
    public
        .parse::<age::ssh::Recipient>()
        .map_err(|_| err(format!("{} keys are not supported (use ssh-ed25519 or ssh-rsa)", pk.algorithm())))?;
    Ok((public, pk.fingerprint(HashAlg::Sha256).to_string(), pk.algorithm().to_string()))
}

/// The public half of a private key file: from the key itself (OpenSSH
/// format stores it in the clear, even when encrypted), else `<path>.pub`.
fn public_of(identity: &Path, text: &str) -> Result<String> {
    if let Ok(k) = PrivateKey::from_openssh(text) {
        return k.public_key().to_openssh().map_err(|e| err(e.to_string()));
    }
    let mut pubf = identity.as_os_str().to_owned();
    pubf.push(".pub");
    std::fs::read_to_string(&pubf)
        .map_err(|_| err(format!("cannot read the public key ({} is missing)", Path::new(&pubf).display())))
}

fn age_identity(identity: &Path, text: &str) -> Result<age::ssh::Identity> {
    age::ssh::Identity::from_buffer(BufReader::new(text.as_bytes()), Some(identity.display().to_string()))
        .map_err(|e| err(format!("not an SSH private key age can use: {e}")))
}

/// Load a private key file (without decrypting it).
pub fn load(identity: &Path) -> Result<SshKey> {
    let text = Zeroizing::new(
        std::fs::read_to_string(identity).map_err(|e| err(format!("reading {}: {e}", identity.display())))?,
    );
    let encrypted = match age_identity(identity, &text)? {
        age::ssh::Identity::Unencrypted(_) => false,
        age::ssh::Identity::Encrypted(_) => true,
        age::ssh::Identity::Unsupported(_) => return Err(err("unsupported key type (use ssh-ed25519 or ssh-rsa)")),
    };
    let (public, fingerprint, algorithm) = describe_public(&public_of(identity, &text)?)?;
    Ok(SshKey { identity: identity.to_path_buf(), public, fingerprint, algorithm, encrypted })
}

/// Encrypt `dek` to an SSH public key.
pub fn wrap(public: &str, dek: &Key) -> Result<Vec<u8>> {
    let r: age::ssh::Recipient = public.parse().map_err(|_| err("invalid SSH recipient"))?;
    age::encrypt(&r, dek.bytes()).map_err(|e| err(format!("encrypting to the SSH key: {e}")))
}

/// Supplies an already-obtained passphrase to age.
#[derive(Clone)]
struct Pass(SecretString);

impl age::Callbacks for Pass {
    fn display_message(&self, _: &str) {}

    fn confirm(&self, _: &str, _: &str, _: Option<&str>) -> Option<bool> {
        None
    }

    fn request_public_string(&self, _: &str) -> Option<String> {
        None
    }

    fn request_passphrase(&self, _: &str) -> Option<SecretString> {
        Some(self.0.clone())
    }
}

fn passphrase_for(identity: &Path, ui: &dyn Prompter) -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var(SSH_PASS_ENV) {
        return Ok(Zeroizing::new(p));
    }
    ui.secret(&format!("Passphrase for SSH key {}: ", identity.display())).ok_or_else(|| {
        err(format!(
            "SSH key {} is passphrase-protected and there is no terminal to ask (set {SSH_PASS_ENV})",
            identity.display()
        ))
    })
}

/// Decrypt a DEK wrapped by [`wrap`] with the private key at `identity`.
pub fn unwrap(identity: &Path, wrapped: &[u8], ui: &dyn Prompter) -> Result<Key> {
    let text = Zeroizing::new(
        std::fs::read_to_string(identity).map_err(|e| err(format!("reading {}: {e}", identity.display())))?,
    );
    let id = age_identity(identity, &text)?;
    let plain = if matches!(id, age::ssh::Identity::Encrypted(_)) {
        let pass = passphrase_for(identity, ui)?;
        age::decrypt(&id.with_callbacks(Pass(SecretString::from(pass.to_string()))), wrapped)
    } else {
        age::decrypt(&id, wrapped)
    }
    .map_err(|e| err(format!("SSH key {} cannot decrypt this database: {e}", identity.display())))?;
    Key::from_bytes(&Zeroizing::new(plain))
}

#[cfg(test)]
pub mod testkeys {
    //! Throwaway SSH keys generated in-process (no ssh-keygen).
    use std::path::{Path, PathBuf};

    use ssh_key::{rand_core::OsRng, Algorithm, LineEnding, PrivateKey};

    /// Write an ed25519 key pair to `dir/name` (+ `.pub`), optionally encrypted.
    pub fn ed25519(dir: &Path, name: &str, passphrase: Option<&str>) -> PathBuf {
        let mut k = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        k.set_comment("test@genome-cli");
        let public = k.public_key().to_openssh().unwrap();
        if let Some(p) = passphrase {
            k = k.encrypt(&mut OsRng, p).unwrap();
        }
        let path = dir.join(name);
        std::fs::write(&path, k.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        std::fs::write(dir.join(format!("{name}.pub")), format!("{public}\n")).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;

    #[test]
    fn unencrypted_round_trip_is_silent() {
        let t = tempfile::tempdir().unwrap();
        let p = testkeys::ed25519(t.path(), "id_ed25519", None);
        let k = load(&p).unwrap();
        assert!(!k.encrypted && k.fingerprint.starts_with("SHA256:") && k.algorithm == "ssh-ed25519");
        let dek = Key::random().unwrap();
        let ui = Scripted::new(false, &[]);
        let back = unwrap(&p, &wrap(&k.public, &dek).unwrap(), &ui).unwrap();
        assert_eq!(back.bytes(), dek.bytes());
        assert!(ui.transcript.borrow().is_empty());
    }

    #[test]
    fn encrypted_key_asks_for_its_passphrase() {
        let t = tempfile::tempdir().unwrap();
        let p = testkeys::ed25519(t.path(), "id_ed25519", Some("hunter22"));
        let k = load(&p).unwrap();
        assert!(k.encrypted);
        let dek = Key::random().unwrap();
        let wrapped = wrap(&k.public, &dek).unwrap();
        let ui = Scripted::new(true, &["hunter22"]);
        assert_eq!(unwrap(&p, &wrapped, &ui).unwrap().bytes(), dek.bytes());
        assert!(ui.transcript.borrow().contains("Passphrase for SSH key"));
        let headless = Scripted::new(false, &[]);
        assert!(unwrap(&p, &wrapped, &headless).err().unwrap().message.contains(SSH_PASS_ENV));
        let wrong = Scripted::new(true, &["nope"]);
        assert!(unwrap(&p, &wrapped, &wrong).is_err());
    }

    #[test]
    fn another_key_cannot_decrypt() {
        let t = tempfile::tempdir().unwrap();
        let a = load(&testkeys::ed25519(t.path(), "a", None)).unwrap();
        let b = testkeys::ed25519(t.path(), "b", None);
        let wrapped = wrap(&a.public, &Key::random().unwrap()).unwrap();
        assert!(unwrap(&b, &wrapped, &Scripted::new(false, &[])).is_err());
    }

    #[test]
    fn public_keys_are_validated() {
        let t = tempfile::tempdir().unwrap();
        let p = testkeys::ed25519(t.path(), "k", None);
        let line = std::fs::read_to_string(t.path().join("k.pub")).unwrap();
        let (_, fp, _) = describe_public(&line).unwrap();
        assert_eq!(fp, load(&p).unwrap().fingerprint);
        assert!(describe_public("ssh-ed25519 notbase64").is_err());
    }
}
