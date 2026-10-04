//! Key management: a random data-encryption key (DEK) per database, wrapped by
//! a key-encryption key (KEK). The wrapped DEK and the KEK's description live
//! in the database's [`Envelope`]; the KEK itself never touches disk.
//!
//! KEK sources:
//! - `keyring`: a random 256-bit KEK kept in the OS credential store (macOS
//!   Keychain via the Security framework, Secret Service on Linux), via the
//!   `keyring` crate.
//! - `passphrase`: Argon2id(passphrase, salt). The passphrase comes from a
//!   session entry cached by `genome db unlock`, then `GENOME_KEY` (CI), then an
//!   interactive prompt.

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{self, hex, unhex, KdfParams, Key};
use crate::error::{AppError, ErrorKind, Result};

pub const KEY_ENV: &str = "GENOME_KEY";
pub const NEW_KEY_ENV: &str = "GENOME_NEW_KEY";
pub const EXPORT_KEY_ENV: &str = "GENOME_EXPORT_KEY";
const KEYRING_SERVICE: &str = "genome-cli";
pub const CIPHER: &str = "xchacha20poly1305";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KekKind {
    Keyring,
    Passphrase,
}

impl KekKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::Passphrase => "passphrase",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Kdf {
    pub alg: String,
    #[serde(flatten)]
    pub params: KdfParams,
    pub salt: String,
}

/// Public (authenticated, unencrypted) key metadata stored with the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub cipher: String,
    pub db_id: String,
    pub kek: KekKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kdf: Option<Kdf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyring_account: Option<String>,
    /// `seal(KEK, "genome-cli dek v1" | db_id, DEK)`, hex.
    pub wrapped_dek: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rekeyed_at: Option<String>,
}

fn dek_aad(db_id: &str) -> Vec<u8> {
    [b"genome-cli dek v1|".as_slice(), db_id.as_bytes()].concat()
}

fn crypto_err(m: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Crypto, m)
}

/// Read a passphrase: `env_var` if set, else an interactive prompt (twice when `confirm`).
pub fn passphrase(env_var: &str, prompt: &str, confirm: bool) -> Result<Zeroizing<String>> {
    if let Ok(v) = std::env::var(env_var) {
        if v.is_empty() {
            return Err(crypto_err(format!("{env_var} is set but empty")));
        }
        return Ok(Zeroizing::new(v));
    }
    if !(std::io::stdin().is_terminal() || std::io::stderr().is_terminal()) {
        return Err(crypto_err(format!(
            "a passphrase is required: set {env_var} or run in a terminal (see docs/security.md)"
        )));
    }
    let p =
        Zeroizing::new(rpassword::prompt_password(prompt).map_err(|e| crypto_err(format!("reading passphrase: {e}")))?);
    if p.is_empty() {
        return Err(crypto_err("empty passphrase"));
    }
    if confirm {
        let again = Zeroizing::new(
            rpassword::prompt_password("Repeat passphrase: ")
                .map_err(|e| crypto_err(format!("reading passphrase: {e}")))?,
        );
        if *again != *p {
            return Err(crypto_err("passphrases do not match"));
        }
    }
    Ok(p)
}

fn keyring_entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| crypto_err(format!("OS keyring: {e}")))
}

fn keyring_get(account: &str) -> Result<Option<Key>> {
    match keyring_entry(account)?.get_secret() {
        Ok(s) => Key::from_bytes(&Zeroizing::new(s)).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(crypto_err(format!("OS keyring: {e}"))),
    }
}

fn keyring_set(account: &str, key: &Key) -> Result<()> {
    keyring_entry(account)?.set_secret(key.bytes()).map_err(|e| crypto_err(format!("OS keyring: {e}")))?;
    // Read back: some backends accept writes they cannot persist.
    match keyring_get(account)? {
        Some(k) if k.bytes() == key.bytes() => Ok(()),
        _ => Err(crypto_err("OS keyring did not store the key")),
    }
}

pub fn keyring_delete(account: &str) -> Result<bool> {
    match keyring_entry(account)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(crypto_err(format!("OS keyring: {e}"))),
    }
}

fn session_account(db_id: &str) -> String {
    format!("session:{db_id}")
}

/// The KEK and how to describe it in an envelope.
struct NewKek {
    kek: Key,
    kind: KekKind,
    kdf: Option<Kdf>,
    account: Option<String>,
}

/// Choose and materialize a KEK for a new envelope. `pref` is `auto`, `keyring`
/// or `passphrase`. `auto`: `pass_env` set -> passphrase; else the OS keyring;
/// else an interactive passphrase.
fn new_kek(pref: &str, db_id: &str, pass_env: &str) -> Result<NewKek> {
    let from_pass = || -> Result<NewKek> {
        let pass = passphrase(pass_env, "New database passphrase: ", true)?;
        let mut salt = [0u8; 16];
        crypto::fill_random(&mut salt)?;
        let params = KdfParams::for_new_key();
        let kek = crypto::derive_key(pass.as_bytes(), &salt, params)?;
        Ok(NewKek {
            kek,
            kind: KekKind::Passphrase,
            kdf: Some(Kdf { alg: "argon2id".into(), params, salt: hex(&salt) }),
            account: None,
        })
    };
    let from_keyring = || -> Result<NewKek> {
        let kek = Key::random()?;
        let account = format!("db:{db_id}");
        keyring_set(&account, &kek)?;
        Ok(NewKek { kek, kind: KekKind::Keyring, kdf: None, account: Some(account) })
    };
    match pref {
        "passphrase" => from_pass(),
        "keyring" => from_keyring(),
        _ if std::env::var_os(pass_env).is_some() => from_pass(),
        _ => from_keyring()
            .or_else(|e| from_pass().map_err(|pe| crypto_err(format!("no key source available ({e}; {pe})")))),
    }
}

impl Envelope {
    /// A new envelope wrapping a fresh random DEK.
    pub fn create(pref: &str) -> Result<(Self, Key)> {
        let mut id = [0u8; 16];
        crypto::fill_random(&mut id)?;
        let db_id = hex(&id);
        let dek = Key::random()?;
        let env = Self::wrap(pref, &db_id, &dek, KEY_ENV, crate::util::now_iso(), None)?;
        Ok((env, dek))
    }

    fn wrap(
        pref: &str,
        db_id: &str,
        dek: &Key,
        pass_env: &str,
        created_at: String,
        rekeyed_at: Option<String>,
    ) -> Result<Self> {
        let k = new_kek(pref, db_id, pass_env)?;
        let wrapped = crypto::seal(&k.kek, &dek_aad(db_id), dek.bytes())?;
        Ok(Self {
            v: 1,
            cipher: CIPHER.into(),
            db_id: db_id.to_string(),
            kek: k.kind,
            kdf: k.kdf,
            keyring_account: k.account,
            wrapped_dek: hex(&wrapped),
            created_at,
            rekeyed_at,
        })
    }

    /// Re-wrap the same DEK under a new KEK (`db rekey`). New passphrases come
    /// from `GENOME_NEW_KEY` or a prompt.
    pub fn rekey(&self, dek: &Key, pref: &str) -> Result<Self> {
        Self::wrap(pref, &self.db_id, dek, NEW_KEY_ENV, self.created_at.clone(), Some(crate::util::now_iso()))
    }

    fn unwrap_with(&self, kek: &Key) -> Result<Key> {
        let plain = crypto::open(kek, &dek_aad(&self.db_id), &unhex(&self.wrapped_dek)?, "database key")
            .map_err(|_| crypto_err("wrong key: the database key could not be unwrapped (wrong passphrase or keyring entry, or a tampered header)"))?;
        Key::from_bytes(&plain)
    }

    /// Derive the passphrase KEK (no unwrap).
    pub fn passphrase_kek(&self, pass: &str) -> Result<Key> {
        let kdf = self.kdf.as_ref().ok_or_else(|| crypto_err("envelope has no KDF parameters"))?;
        crypto::derive_key(pass.as_bytes(), &unhex(&kdf.salt)?, kdf.params)
    }

    /// Obtain the KEK from its source and unwrap the DEK.
    pub fn unlock(&self) -> Result<Key> {
        match self.kek {
            KekKind::Keyring => {
                let account = self.keyring_account.as_deref().unwrap_or_default();
                let kek = keyring_get(account)?.ok_or_else(|| {
                    crypto_err(format!("OS keyring has no entry '{KEYRING_SERVICE}/{account}' for this database"))
                })?;
                self.unwrap_with(&kek)
            }
            KekKind::Passphrase => {
                // A session cached by `db unlock` (only when no explicit env key is given).
                if std::env::var_os(KEY_ENV).is_none() {
                    if let Ok(Some(kek)) = keyring_get(&session_account(&self.db_id)) {
                        if let Ok(dek) = self.unwrap_with(&kek) {
                            return Ok(dek);
                        }
                    }
                }
                let pass = passphrase(KEY_ENV, "Database passphrase: ", false)?;
                self.unwrap_with(&self.passphrase_kek(&pass)?)
            }
        }
    }

    /// Cache the passphrase-derived KEK in the OS keyring (`db unlock`).
    pub fn cache_session(&self) -> Result<()> {
        if self.kek != KekKind::Passphrase {
            return Err(AppError::usage("this database's key is already kept in the OS keyring"));
        }
        let pass = passphrase(KEY_ENV, "Database passphrase: ", false)?;
        let kek = self.passphrase_kek(&pass)?;
        self.unwrap_with(&kek)?;
        keyring_set(&session_account(&self.db_id), &kek)
    }

    /// Remove a cached session (`db lock`). Returns whether one existed.
    pub fn clear_session(&self) -> Result<bool> {
        keyring_delete(&session_account(&self.db_id))
    }
}

// ---------------------------------------------------------------------------
// Passphrase-sealed exports (`--encrypt-output`, `genome decrypt`)
// ---------------------------------------------------------------------------

pub const EXPORT_MAGIC: &[u8; 8] = b"GNMEXPT1";

/// `GNMEXPT1 | u32 len | {"kdf":...} | seal(Argon2id(passphrase), prefix, data)`.
pub fn seal_export(data: &[u8]) -> Result<Vec<u8>> {
    let pass = passphrase(EXPORT_KEY_ENV, "Passphrase for the encrypted output: ", true)?;
    let mut salt = [0u8; 16];
    crypto::fill_random(&mut salt)?;
    let params = KdfParams::for_new_key();
    let key = crypto::derive_key(pass.as_bytes(), &salt, params)?;
    let meta = serde_json::to_vec(&Kdf { alg: "argon2id".into(), params, salt: hex(&salt) })?;
    let mut out = Vec::with_capacity(data.len() + meta.len() + 64);
    out.extend_from_slice(EXPORT_MAGIC);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
    let body = crypto::seal(&key, &out, data)?;
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn open_export(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let bad = || AppError::invalid("not a genome-cli encrypted export");
    if bytes.len() < 12 || &bytes[..8] != EXPORT_MAGIC {
        return Err(bad());
    }
    let n = u32::from_le_bytes(bytes[8..12].try_into().expect("4")) as usize;
    let prefix = bytes.get(..12 + n).ok_or_else(bad)?;
    let kdf: Kdf = serde_json::from_slice(&prefix[12..])?;
    let pass = passphrase(EXPORT_KEY_ENV, "Passphrase: ", false)?;
    let key = crypto::derive_key(pass.as_bytes(), &unhex(&kdf.salt)?, kdf.params)?;
    crypto::open(&key, prefix, &bytes[12 + n..], "encrypted export")
}
