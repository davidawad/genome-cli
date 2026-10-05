//! Key management: a random data-encryption key (DEK) per database, wrapped
//! once per key slot. Any one slot decrypts the database; the genotype stores
//! and the audit log use the same DEK, so adding or removing a slot never
//! re-encrypts data.
//!
//! Slot kinds:
//! - `ssh` (default): the DEK encrypted to the user's SSH public key with age
//!   ([`crate::sshkey`]); the private key decrypts it.
//! - `file`: a random 256-bit KEK in an owner-only key file outside the data
//!   directory, like an ssh key ([`crate::keyfile`]). Added next to an `ssh`
//!   slot when the SSH key has a passphrase, so daily use never prompts.
//! - `passphrase`: Argon2id(passphrase, salt), from `GENOME_KEY` or a prompt.
//! - `keyring`: legacy (0.2): a KEK in the OS credential store. Still read so
//!   `genome db rekey --to ssh` can migrate away; new ones only on request.
//!
//! Unlock order: key file -> SSH key -> `GENOME_KEY` -> legacy keyring ->
//! passphrase prompt. The OS keyring is never touched unless a slot (or a
//! `db unlock` session) uses it.

use std::io::IsTerminal;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{self, hex, unhex, KdfParams, Key};
use crate::error::{AppError, ErrorKind, Result};
use crate::keyfile;
use crate::platform::keystore::{self, SERVICE as KEYRING_SERVICE};
use crate::prompt::{Prompter, Tty};
use crate::sshkey::{self, SshKey};

pub const KEY_ENV: &str = "GENOME_KEY";
pub const NEW_KEY_ENV: &str = "GENOME_NEW_KEY";
pub const EXPORT_KEY_ENV: &str = "GENOME_EXPORT_KEY";
pub const CIPHER: &str = "xchacha20poly1305";
pub const ENVELOPE_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    Ssh,
    File,
    Passphrase,
    Keyring,
}

impl SlotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::File => "file",
            Self::Passphrase => "passphrase",
            Self::Keyring => "keyring",
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

/// One way to unwrap the DEK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub id: String,
    pub kind: SlotKind,
    /// `ssh`: age ciphertext of the DEK; others: `seal(KEK, "genome-cli dek v1" | db_id, DEK)`. Hex.
    pub wrapped_dek: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<Kdf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyring_account: Option<String>,
    /// `file`: an explicit `GENOME_KEY_FILE` (default key files are found by `db_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_fingerprint: Option<String>,
    /// `ssh`: where the private key was when the slot was made (a hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_identity: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

/// Public (authenticated, unencrypted) key metadata stored with the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub cipher: String,
    pub db_id: String,
    #[serde(default)]
    pub slots: Vec<Slot>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rekeyed_at: Option<String>,
    // Version 1 (one KEK per database): folded into `slots` by `normalize`, never written.
    #[serde(default, skip_serializing)]
    kek: Option<SlotKind>,
    #[serde(default, skip_serializing)]
    kdf: Option<Kdf>,
    #[serde(default, skip_serializing)]
    keyring_account: Option<String>,
    #[serde(default, skip_serializing)]
    wrapped_dek: Option<String>,
}

fn dek_aad(db_id: &str) -> Vec<u8> {
    [b"genome-cli dek v1|".as_slice(), db_id.as_bytes()].concat()
}

fn crypto_err(m: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Crypto, m)
}

fn new_id(kind: SlotKind) -> Result<String> {
    let mut b = [0u8; 4];
    crypto::fill_random(&mut b)?;
    Ok(format!("{}-{}", kind.as_str(), hex(&b)))
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

fn session_account(db_id: &str) -> String {
    format!("session:{db_id}")
}

// ---------------------------------------------------------------------------
// Making slots
// ---------------------------------------------------------------------------

fn sealed_slot(kind: SlotKind, kek: &Key, db_id: &str, dek: &Key) -> Result<Slot> {
    Ok(Slot {
        id: new_id(kind)?,
        kind,
        wrapped_dek: hex(&crypto::seal(kek, &dek_aad(db_id), dek.bytes())?),
        kdf: None,
        keyring_account: None,
        key_file: None,
        ssh_public_key: None,
        ssh_fingerprint: None,
        ssh_identity: None,
        created_at: crate::util::now_iso(),
    })
}

/// A passphrase slot; the passphrase comes from `pass_env` or a prompt.
pub fn passphrase_slot(db_id: &str, dek: &Key, pass_env: &str) -> Result<Slot> {
    let pass = passphrase(pass_env, "New database passphrase: ", true)?;
    let mut salt = [0u8; 16];
    crypto::fill_random(&mut salt)?;
    let params = KdfParams::for_new_key();
    let kek = crypto::derive_key(pass.as_bytes(), &salt, params)?;
    let slot = sealed_slot(SlotKind::Passphrase, &kek, db_id, dek)?;
    Ok(Slot { kdf: Some(Kdf { alg: "argon2id".into(), params, salt: hex(&salt) }), ..slot })
}

/// A key file slot (a new random KEK in the default or `GENOME_KEY_FILE` key file).
pub fn file_slot(db_id: &str, dek: &Key) -> Result<Slot> {
    let (kek, key_file) = keyfile::create(db_id)?;
    Ok(Slot { key_file, ..sealed_slot(SlotKind::File, &kek, db_id, dek)? })
}

fn keyring_slot(db_id: &str, dek: &Key) -> Result<Slot> {
    let kek = Key::random()?;
    let account = format!("db:{db_id}");
    keystore::set(&account, &kek)?;
    Ok(Slot { keyring_account: Some(account), ..sealed_slot(SlotKind::Keyring, &kek, db_id, dek)? })
}

/// An SSH slot for a public key line (`identity`: where its private key lives, if known).
pub fn ssh_slot(public: &str, identity: Option<&std::path::Path>, dek: &Key) -> Result<Slot> {
    let (public, fingerprint, _) = sshkey::describe_public(public)?;
    let wrapped = sshkey::wrap(&public, dek)?;
    Ok(Slot {
        id: new_id(SlotKind::Ssh)?,
        kind: SlotKind::Ssh,
        wrapped_dek: hex(&wrapped),
        kdf: None,
        keyring_account: None,
        key_file: None,
        ssh_public_key: Some(public),
        ssh_fingerprint: Some(fingerprint),
        ssh_identity: identity.map(|p| p.display().to_string()),
        created_at: crate::util::now_iso(),
    })
}

/// Slots for an SSH key: the key itself, plus a key file when the key has a
/// passphrase (so everyday commands never prompt; the SSH key is the recovery key).
fn ssh_slots(k: &SshKey, db_id: &str, dek: &Key) -> Result<Vec<Slot>> {
    let mut slots = vec![ssh_slot(&k.public, Some(&k.identity), dek)?];
    if k.encrypted {
        slots.push(file_slot(db_id, dek)?);
    }
    Ok(slots)
}

fn no_ssh_key(notes: &[String]) -> AppError {
    let looked: Vec<String> = sshkey::candidates().iter().map(|p| p.display().to_string()).collect();
    let extra = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
    crypto_err(format!(
        "no usable SSH key (looked at {}){extra}; set {} to a private key, or use --kek file",
        looked.join(", "),
        sshkey::SSH_KEY_ENV
    ))
}

/// First-run choice: the SSH key if there is one and the user agrees, else a key file.
fn auto_slots(db_id: &str, dek: &Key, ui: &dyn Prompter) -> Result<Vec<Slot>> {
    let (found, _) = sshkey::discover();
    if let Some(k) = found {
        ui.say(&format!(
            "genome: your data will be encrypted with your SSH key {} ({}, {}).",
            k.identity.display(),
            k.algorithm,
            k.fingerprint
        ));
        if ui.confirm("Encrypt with this SSH key?", true) {
            return ssh_slots(&k, db_id, dek);
        }
    }
    file_slot(db_id, dek).map(|s| vec![s])
}

/// Slots for a new or re-keyed database. `pref`: `auto` (`pass_env` set ->
/// passphrase; else the SSH key, asking first; else a key file), `ssh`,
/// `file`, `passphrase` or `keyring`.
pub fn choose_slots(pref: &str, db_id: &str, dek: &Key, pass_env: &str, ui: &dyn Prompter) -> Result<Vec<Slot>> {
    match pref {
        "passphrase" => passphrase_slot(db_id, dek, pass_env).map(|s| vec![s]),
        "file" => file_slot(db_id, dek).map(|s| vec![s]),
        "keyring" => keyring_slot(db_id, dek).map(|s| vec![s]),
        "ssh" => match sshkey::discover() {
            (Some(k), _) => ssh_slots(&k, db_id, dek),
            (None, notes) => Err(no_ssh_key(&notes)),
        },
        _ if std::env::var_os(pass_env).is_some() => passphrase_slot(db_id, dek, pass_env).map(|s| vec![s]),
        _ => auto_slots(db_id, dek, ui),
    }
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// The result of unlocking: the DEK and which slot opened it.
pub struct Unlocked {
    pub dek: Key,
    pub slot: String,
}

type Step<'a> = &'a dyn Fn(&mut Vec<String>) -> Option<Unlocked>;

impl Envelope {
    /// A new envelope wrapping a fresh random DEK.
    pub fn create(pref: &str, ui: &dyn Prompter) -> Result<(Self, Key)> {
        let mut id = [0u8; 16];
        crypto::fill_random(&mut id)?;
        let db_id = hex(&id);
        let dek = Key::random()?;
        let slots = choose_slots(pref, &db_id, &dek, KEY_ENV, ui)?;
        let env = Self {
            v: ENVELOPE_VERSION,
            cipher: CIPHER.into(),
            db_id,
            slots,
            created_at: crate::util::now_iso(),
            rekeyed_at: None,
            kek: None,
            kdf: None,
            keyring_account: None,
            wrapped_dek: None,
        };
        Ok((env, dek))
    }

    /// Fold a version-1 envelope (one KEK) into a single slot.
    pub fn normalize(mut self) -> Self {
        if self.slots.is_empty() {
            if let (Some(kind), Some(wrapped)) = (self.kek, self.wrapped_dek.take()) {
                self.slots.push(Slot {
                    id: format!("{}-legacy", kind.as_str()),
                    kind,
                    wrapped_dek: wrapped,
                    kdf: self.kdf.take(),
                    keyring_account: self.keyring_account.take(),
                    key_file: None,
                    ssh_public_key: None,
                    ssh_fingerprint: None,
                    ssh_identity: None,
                    created_at: self.created_at.clone(),
                });
            }
        }
        self.v = ENVELOPE_VERSION;
        self.kek = None;
        self
    }

    /// Replace every slot (`db rekey`). New passphrases come from
    /// `GENOME_NEW_KEY` or a prompt; a rotated default key file is staged as
    /// `<file>.new` until [`Envelope::commit_keys`] runs after the envelope is saved.
    pub fn rekey(&self, dek: &Key, pref: &str, ui: &dyn Prompter) -> Result<Self> {
        let slots = choose_slots(pref, &self.db_id, dek, NEW_KEY_ENV, ui)?;
        Ok(Self { slots, rekeyed_at: Some(crate::util::now_iso()), ..self.clone() })
    }

    /// Kinds present, e.g. `ssh+file`.
    pub fn kinds(&self) -> String {
        self.slots.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>().join("+")
    }

    pub fn has(&self, kind: SlotKind) -> bool {
        self.slots.iter().any(|s| s.kind == kind)
    }

    /// Add a slot (the caller re-seals the database).
    pub fn add_slot(&mut self, slot: Slot) {
        self.slots.push(slot);
    }

    /// Remove the slot with this id (or the only slot of this kind). The last
    /// slot cannot be removed.
    pub fn remove_slot(&mut self, which: &str) -> Result<Slot> {
        let matches: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].id == which || self.slots[i].kind.as_str() == which)
            .collect();
        let i = match matches.as_slice() {
            [i] => *i,
            [] => return Err(AppError::not_found(format!("no key slot '{which}' (see `genome key status`)"))),
            _ => return Err(AppError::usage(format!("several '{which}' slots: name one by id (`genome key status`)"))),
        };
        if self.slots.len() == 1 {
            return Err(AppError::usage("refusing to remove the last key slot: the data would be unrecoverable"));
        }
        Ok(self.slots.remove(i))
    }

    /// After the envelope is saved: move staged key files into place.
    pub fn commit_keys(&self) -> Result<()> {
        for s in self.slots.iter().filter(|s| s.kind == SlotKind::File) {
            keyfile::commit(&self.db_id, s.key_file.as_deref())?;
        }
        Ok(())
    }

    /// The key file of a `file` slot.
    pub fn key_file_path(&self, slot: &Slot) -> PathBuf {
        keyfile::resolve(&self.db_id, slot.key_file.as_deref())
    }

    fn unwrap_with(&self, slot: &Slot, kek: &Key) -> Result<Key> {
        let plain = crypto::open(kek, &dek_aad(&self.db_id), &unhex(&slot.wrapped_dek)?, "database key")
            .map_err(|_| crypto_err("wrong key: the database key could not be unwrapped"))?;
        Key::from_bytes(&plain)
    }

    /// Derive a passphrase slot's KEK.
    fn passphrase_kek(slot: &Slot, pass: &str) -> Result<Key> {
        let kdf = slot.kdf.as_ref().ok_or_else(|| crypto_err("passphrase slot has no KDF parameters"))?;
        crypto::derive_key(pass.as_bytes(), &unhex(&kdf.salt)?, kdf.params)
    }

    fn slots_of(&self, kind: SlotKind) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(move |s| s.kind == kind)
    }

    /// Unwrap the DEK through the terminal.
    pub fn unlock(&self) -> Result<Key> {
        self.unlock_with(&Tty).map(|u| u.dek)
    }

    /// Try each slot in order (key file, SSH, `GENOME_KEY`, keyring, prompt).
    pub fn unlock_with(&self, ui: &dyn Prompter) -> Result<Unlocked> {
        let mut why = Vec::new();
        let steps: [Step; 5] = [
            &|w| self.try_files(w),
            &|w| self.try_ssh(ui, w),
            &|w| self.try_env_passphrase(w),
            &|w| self.try_keyring(w),
            &|w| self.try_prompt(w),
        ];
        for step in steps {
            if let Some(u) = step(&mut why) {
                return Ok(u);
            }
        }
        if why.is_empty() {
            why.push("the database has no key slots".into());
        }
        Err(crypto_err(format!("cannot unlock the database: {}", why.join("; "))))
    }

    fn found(&self, slot: &Slot, r: Result<Key>, why: &mut Vec<String>) -> Option<Unlocked> {
        match r {
            Ok(dek) => Some(Unlocked { dek, slot: slot.id.clone() }),
            Err(e) => {
                why.push(format!("{}: {}", slot.id, e.message));
                None
            }
        }
    }

    fn try_files(&self, why: &mut Vec<String>) -> Option<Unlocked> {
        self.slots_of(SlotKind::File).find_map(|s| {
            let path = self.key_file_path(s);
            if !path.exists() {
                why.push(format!("{}: key file {} is missing", s.id, path.display()));
                return None;
            }
            let r = keyfile::load_candidates(&path).and_then(|keys| {
                keys.iter()
                    .find_map(|k| self.unwrap_with(s, k).ok())
                    .ok_or_else(|| crypto_err(format!("key file {} does not unlock this database", path.display())))
            });
            self.found(s, r, why)
        })
    }

    fn ssh_identities(slot: &Slot) -> Vec<PathBuf> {
        let mut ids = Vec::new();
        if std::env::var_os(sshkey::SSH_KEY_ENV).is_none() {
            ids.extend(slot.ssh_identity.as_ref().map(PathBuf::from));
        }
        ids.extend(sshkey::candidates());
        ids.dedup();
        ids.into_iter().filter(|p| p.exists()).collect()
    }

    fn try_ssh(&self, ui: &dyn Prompter, why: &mut Vec<String>) -> Option<Unlocked> {
        self.slots_of(SlotKind::Ssh).find_map(|s| {
            let want = s.ssh_fingerprint.as_deref().unwrap_or_default();
            let Some(id) =
                Self::ssh_identities(s).into_iter().find(|p| sshkey::load(p).is_ok_and(|k| k.fingerprint == want))
            else {
                why.push(format!("{}: SSH private key {want} not found (set {})", s.id, sshkey::SSH_KEY_ENV));
                return None;
            };
            let r = unhex(&s.wrapped_dek).and_then(|w| sshkey::unwrap(&id, &w, ui));
            self.found(s, r, why)
        })
    }

    fn try_env_passphrase(&self, why: &mut Vec<String>) -> Option<Unlocked> {
        let pass = Zeroizing::new(std::env::var(KEY_ENV).ok()?);
        self.slots_of(SlotKind::Passphrase).find_map(|s| {
            let r = Self::passphrase_kek(s, &pass)
                .and_then(|kek| self.unwrap_with(s, &kek))
                .map_err(|_| crypto_err(format!("wrong key: {KEY_ENV} does not unlock this database")));
            self.found(s, r, why)
        })
    }

    fn try_keyring(&self, why: &mut Vec<String>) -> Option<Unlocked> {
        if let Some(u) = self.try_session() {
            return Some(u);
        }
        self.slots_of(SlotKind::Keyring).find_map(|s| {
            let account = s.keyring_account.as_deref().unwrap_or_default();
            let r = keystore::get(account)
                .and_then(|k| {
                    k.ok_or_else(|| crypto_err(format!("OS keyring has no entry '{KEYRING_SERVICE}/{account}'")))
                })
                .and_then(|kek| self.unwrap_with(s, &kek));
            self.found(s, r, why)
        })
    }

    /// A passphrase KEK cached by `db unlock` (only if one was cached).
    fn try_session(&self) -> Option<Unlocked> {
        if std::env::var_os(KEY_ENV).is_some() || !keyfile::session_marker(&self.db_id).exists() {
            return None;
        }
        let kek = keystore::get(&session_account(&self.db_id)).ok()??;
        self.slots_of(SlotKind::Passphrase)
            .find_map(|s| self.unwrap_with(s, &kek).ok().map(|dek| Unlocked { dek, slot: s.id.clone() }))
    }

    fn try_prompt(&self, why: &mut Vec<String>) -> Option<Unlocked> {
        if std::env::var_os(KEY_ENV).is_some() {
            return None;
        }
        let s = self.slots_of(SlotKind::Passphrase).next()?;
        let r = passphrase(KEY_ENV, "Database passphrase: ", false)
            .and_then(|p| Self::passphrase_kek(s, &p))
            .and_then(|kek| self.unwrap_with(s, &kek));
        self.found(s, r, why)
    }

    /// Cache the passphrase-derived KEK in the OS keyring (`db unlock`, opt-in).
    pub fn cache_session(&self) -> Result<()> {
        let s = self.slots_of(SlotKind::Passphrase).next().ok_or_else(|| {
            AppError::usage(format!("this database has no passphrase to cache (keys: {})", self.kinds()))
        })?;
        let pass = passphrase(KEY_ENV, "Database passphrase: ", false)?;
        let kek = Self::passphrase_kek(s, &pass)?;
        self.unwrap_with(s, &kek)?;
        keystore::set(&session_account(&self.db_id), &kek)?;
        let marker = keyfile::session_marker(&self.db_id);
        crate::platform::perms::private_dir(marker.parent().expect("key dir"))?;
        crate::platform::perms::create_private(&marker)?;
        Ok(())
    }

    /// Remove a cached session (`db lock`). Returns whether one existed. The
    /// OS keyring is only touched when `db unlock` left a session marker.
    pub fn clear_session(&self) -> Result<bool> {
        let marker = keyfile::session_marker(&self.db_id);
        if !marker.exists() {
            return Ok(false);
        }
        let _ = std::fs::remove_file(&marker);
        keystore::delete(&session_account(&self.db_id))
    }

    /// Remove what only `old` used and `self` no longer does: keyring
    /// entries, default key files, a cached passphrase session.
    pub fn retire_unused(&self, old: &Envelope) {
        for s in old.slots.iter().filter(|s| !self.slots.iter().any(|n| n.id == s.id)) {
            retire_slot(&old.db_id, s, self.has(SlotKind::File));
        }
        if !self.has(SlotKind::Passphrase) {
            let _ = old.clear_session();
        }
    }
}

/// Clean up after a slot that is gone (best effort).
pub fn retire_slot(db_id: &str, s: &Slot, file_still_used: bool) {
    match s.kind {
        SlotKind::Keyring => {
            let _ = keystore::delete(s.keyring_account.as_deref().unwrap_or_default());
        }
        SlotKind::File if !file_still_used => {
            let _ = keyfile::retire(db_id, s.key_file.as_deref());
        }
        _ => {}
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

#[cfg(test)]
pub mod testenv {
    //! Process-global env overrides for unit tests, serialized by a lock.
    use std::path::PathBuf;

    use super::*;

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub struct Sandbox {
        pub dir: tempfile::TempDir,
        pub ssh: PathBuf,
        _g: std::sync::MutexGuard<'static, ()>,
    }

    /// Fresh SSH dir and key dir in a tempdir; no OS keyring; no env keys.
    pub fn sandbox() -> Sandbox {
        let g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::env::set_var(sshkey::SSH_DIR_ENV, &ssh);
        std::env::set_var("GENOME_KEY_DIR", dir.path().join("keys"));
        std::env::set_var(keystore::NO_KEYRING_ENV, "1");
        for v in [KEY_ENV, NEW_KEY_ENV, sshkey::SSH_KEY_ENV, sshkey::SSH_PASS_ENV, keyfile::KEY_FILE_ENV] {
            std::env::remove_var(v);
        }
        Sandbox { dir, ssh, _g: g }
    }
}

#[cfg(test)]
mod tests {
    use super::testenv::sandbox;
    use super::*;
    use crate::prompt::Scripted;
    use crate::sshkey::testkeys;

    #[test]
    fn first_run_uses_the_ssh_key_after_asking() {
        let sb = sandbox();
        testkeys::ed25519(&sb.ssh, "id_ed25519", None);
        let ui = Scripted::new(true, &["y"]);
        let (env, dek) = Envelope::create("auto", &ui).unwrap();
        assert_eq!(env.kinds(), "ssh");
        assert!(ui.transcript.borrow().contains("Encrypt with this SSH key?"));
        let u = env.unlock_with(&Scripted::new(false, &[])).unwrap();
        assert_eq!((u.dek.bytes(), u.slot.starts_with("ssh-")), (dek.bytes(), true));
    }

    #[test]
    fn declining_or_no_ssh_key_means_a_key_file() {
        let sb = sandbox();
        let (env, _) = Envelope::create("auto", &Scripted::new(false, &[])).unwrap();
        assert_eq!(env.kinds(), "file");
        testkeys::ed25519(&sb.ssh, "id_ed25519", None);
        let (env, _) = Envelope::create("auto", &Scripted::new(true, &["n"])).unwrap();
        assert_eq!(env.kinds(), "file");
        let (env, _) = Envelope::create("auto", &Scripted::new(false, &[])).unwrap();
        assert_eq!(env.kinds(), "ssh", "non-interactive runs proceed with the SSH key");
    }

    #[test]
    fn passphrase_protected_ssh_key_adds_a_key_file_for_daily_use() {
        let sb = sandbox();
        let key = testkeys::ed25519(&sb.ssh, "id_ed25519", Some("ssh-pass"));
        let (env, dek) = Envelope::create("auto", &Scripted::new(true, &["y"])).unwrap();
        assert_eq!(env.kinds(), "ssh+file");
        let u = env.unlock_with(&Scripted::new(false, &[])).unwrap();
        assert!(u.slot.starts_with("file-"), "daily use never prompts");
        // Lose the key file: the SSH key (and its passphrase) recovers.
        let file = env.slots.iter().find(|s| s.kind == SlotKind::File).unwrap();
        std::fs::remove_file(env.key_file_path(file)).unwrap();
        let ui = Scripted::new(true, &["ssh-pass"]);
        let u = env.unlock_with(&ui).unwrap();
        assert_eq!((u.dek.bytes(), u.slot.starts_with("ssh-")), (dek.bytes(), true));
        assert!(ui.transcript.borrow().contains(&key.display().to_string()));
    }

    #[test]
    fn slots_add_and_remove_but_never_the_last() {
        let sb = sandbox();
        let other = testkeys::ed25519(&sb.ssh, "laptop", None);
        let (mut env, dek) = Envelope::create("file", &Scripted::new(false, &[])).unwrap();
        let public = std::fs::read_to_string(sb.ssh.join("laptop.pub")).unwrap();
        env.add_slot(ssh_slot(&public, Some(&other), &dek).unwrap());
        let file = env.remove_slot("file").unwrap();
        assert_eq!(file.kind, SlotKind::File);
        assert!(env.remove_slot("ssh").err().unwrap().message.contains("last key slot"));
        assert_eq!(env.unlock_with(&Scripted::new(false, &[])).unwrap().dek.bytes(), dek.bytes());
    }

    #[test]
    fn version_1_envelopes_become_one_slot() {
        let v1 = r#"{"v":1,"cipher":"xchacha20poly1305","db_id":"ab","kek":"keyring",
            "keyring_account":"db:ab","wrapped_dek":"00","created_at":"2026-10-04T00:00:00Z"}"#;
        let env: Envelope = serde_json::from_str(v1).unwrap();
        let env = env.normalize();
        assert_eq!((env.v, env.kinds().as_str()), (2, "keyring"));
        assert_eq!(env.slots[0].keyring_account.as_deref(), Some("db:ab"));
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"kek\"") && json.contains("\"slots\""), "{json}");
    }

    #[test]
    fn nothing_matches_explains_every_slot() {
        let sb = sandbox();
        let k = testkeys::ed25519(&sb.ssh, "id_ed25519", None);
        let (env, _) = Envelope::create("ssh", &Scripted::new(false, &[])).unwrap();
        std::fs::remove_file(&k).unwrap();
        let e = env.unlock_with(&Scripted::new(false, &[])).err().unwrap();
        assert!(e.message.contains("SSH private key SHA256:"), "{}", e.message);
    }
}
