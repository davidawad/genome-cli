//! `genome key ...`: list, add and remove the key slots that can decrypt the
//! database. Every slot wraps the same data key, so nothing is re-encrypted.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::KeyCmd;
use crate::context::Ctx;
use crate::crypto::Key;
use crate::db::{self, DbFile};
use crate::error::{AppError, Result};
use crate::keys::{self, Envelope, Slot, SlotKind};
use crate::output::{to_record, Report};
use crate::platform::perms::{check_private, Access};
use crate::sshkey;

pub fn run(ctx: &Ctx, cmd: KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Status => status(ctx),
        KeyCmd::AddSsh { public_key } => modify(ctx, "key add-ssh", |env, dek| add_ssh(env, dek, &public_key)),
        KeyCmd::AddPassphrase => modify(ctx, "key add-passphrase", |env, dek| {
            let slot = keys::passphrase_slot(&env.db_id, dek, keys::NEW_KEY_ENV)?;
            Ok(added(env, slot))
        }),
        KeyCmd::AddFile => modify(ctx, "key add-file", |env, dek| {
            if env.has(SlotKind::File) {
                return Err(AppError::usage("this database already has a key file slot"));
            }
            let slot = keys::file_slot(&env.db_id, dek)?;
            Ok(added(env, slot))
        }),
        KeyCmd::Remove { slot } => modify(ctx, "key remove", |env, _| {
            let gone = env.remove_slot(&slot)?;
            Ok(format!("removed key slot {} ({})", gone.id, gone.kind.as_str()))
        }),
    }
}

fn added(env: &mut Envelope, slot: Slot) -> String {
    let msg = format!("added key slot {} ({})", slot.id, slot.kind.as_str());
    env.add_slot(slot);
    msg
}

/// `public_key`: a `.pub` file or the key line itself.
fn add_ssh(env: &mut Envelope, dek: &Key, public_key: &str) -> Result<String> {
    let path = Path::new(public_key);
    let (line, identity) = if path.is_file() {
        let line = std::fs::read_to_string(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
        let private = path.extension().filter(|e| *e == "pub").map(|_| path.with_extension(""));
        (line, private.filter(|p| p.is_file()))
    } else {
        (public_key.to_string(), None)
    };
    let slot = keys::ssh_slot(&line, identity.as_deref(), dek)?;
    if env.slots.iter().any(|s| s.ssh_fingerprint == slot.ssh_fingerprint) {
        return Err(AppError::usage(format!(
            "SSH key {} can already decrypt this database",
            slot.ssh_fingerprint.unwrap_or_default()
        )));
    }
    Ok(added(env, slot))
}

/// Unlock, change the slots, re-seal the database, then tidy up and record.
fn modify(ctx: &Ctx, what: &str, f: impl FnOnce(&mut Envelope, &Key) -> Result<String>) -> Result<()> {
    if db::inspect(&ctx.db_path) != DbFile::Sealed {
        return Err(AppError::usage(format!("{} is not an encrypted database", ctx.db_path.display())));
    }
    let mut d = ctx.db()?;
    let old = d.envelope().cloned().expect("sealed");
    let dek = d.dek().cloned().expect("sealed");
    let mut env = old.clone();
    let msg = f(&mut env, &dek)?;
    d.set_envelope(env.clone())?;
    drop(d);
    env.commit_keys()?;
    env.retire_unused(&old);
    crate::setup::record(ctx, &env);
    ctx.info(&format!("genome: {msg}"));
    ctx.audit(what, json!({"keys": env.kinds()}))?;
    status(ctx)
}

fn ssh_row(s: &Slot) -> (Value, Option<String>) {
    let fp = s.ssh_fingerprint.clone().unwrap_or_default();
    let here = s
        .ssh_identity
        .as_ref()
        .map(PathBuf::from)
        .into_iter()
        .chain(sshkey::candidates())
        .find(|p| sshkey::load(p).is_ok_and(|k| k.fingerprint == fp));
    let detail = match &here {
        Some(p) => format!("{fp} ({})", p.display()),
        None => format!("{fp} (private key not on this machine)"),
    };
    let row = json!({"check": format!("key {}", s.id), "status": if here.is_some() { "ok" } else { "elsewhere" },
        "detail": detail, "purpose": "SSH key (age ssh recipient)", "hint": null});
    (row, None)
}

fn file_row(env: &Envelope, s: &Slot) -> (Value, Option<String>) {
    let path = env.key_file_path(s);
    let access = check_private(&path);
    let warning = match &access {
        Access::Open(why) => Some(format!("key file {} is accessible to other users ({why})", path.display())),
        Access::Missing => Some(format!("key file {} is missing: that slot cannot unlock", path.display())),
        _ => None,
    };
    let hint = matches!(access, Access::Open(_)).then(|| crate::keyfile::fix_hint(&path));
    let row = json!({"check": format!("key {}", s.id), "status": access.status(),
        "detail": path, "purpose": "key file (owner-only, outside the data dir)", "hint": hint});
    (row, warning)
}

fn passphrase_row(s: &Slot) -> (Value, Option<String>) {
    let kdf = s.kdf.as_ref().map(|k| format!("argon2id m={}KiB t={} p={}", k.params.m, k.params.t, k.params.p));
    let row = json!({"check": format!("key {}", s.id), "status": "passphrase", "detail": kdf,
        "purpose": "passphrase (GENOME_KEY or a prompt)", "hint": null});
    (row, None)
}

fn keyring_row(s: &Slot) -> (Value, Option<String>) {
    let row = json!({"check": format!("key {}", s.id), "status": "unsupported",
        "detail": "OS keychain key from genome-cli 0.2 or earlier", "purpose": "no longer supported", "hint": null});
    (row, Some(keys::KEYCHAIN_UNSUPPORTED.into()))
}

/// One row per key slot, plus warnings (used by `key status`, `db status`, `doctor`).
pub fn slot_rows(env: &Envelope) -> (Vec<Value>, Vec<String>) {
    let mut rows = vec![json!({"check": "keys", "status": env.kinds(),
        "detail": format!("{} data key; any one of {} slot(s) decrypts it", env.cipher, env.slots.len()),
        "purpose": "envelope encryption (random per-database DEK)", "hint": null})];
    let mut warnings = Vec::new();
    for s in &env.slots {
        let (row, warning) = match s.kind {
            SlotKind::Ssh => ssh_row(s),
            SlotKind::File => file_row(env, s),
            SlotKind::Passphrase => passphrase_row(s),
            SlotKind::Keyring => keyring_row(s),
        };
        rows.push(row);
        warnings.extend(warning);
    }
    (rows, warnings)
}

fn status(ctx: &Ctx) -> Result<()> {
    let env = match db::inspect(&ctx.db_path) {
        DbFile::Sealed => db::read_envelope(&ctx.db_path)?,
        _ => return Err(AppError::usage(format!("{} is not an encrypted database", ctx.db_path.display()))),
    };
    let (rows, warnings) = slot_rows(&env);
    let rows = rows.iter().map(to_record).collect();
    ctx.emit(
        &Report::new("key-status", rows)
            .table_columns(&["check", "status", "detail"])
            .warnings(warnings)
            .meta("config", json!(ctx.resolved.config_path)),
    )
}
