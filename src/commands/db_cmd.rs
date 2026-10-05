//! `genome db ...` (encryption at rest), `genome audit log`, `genome decrypt`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cli::{AuditCmd, DbCmd, DecryptArgs};
use crate::context::{Ctx, PLAINTEXT_WARNING};
use crate::crypto::{self, Key, SealedReader};
use crate::db::{self, Db, DbFile};
use crate::error::{AppError, ErrorKind, Result};
use crate::gtstore;
use crate::keys::Envelope;
use crate::output::{to_record, Report};
use crate::prompt::Tty;
use crate::store;

pub fn run(ctx: &Ctx, cmd: DbCmd) -> Result<()> {
    match cmd {
        DbCmd::Init { encrypt: _, kek } => init(ctx, kek.as_deref()),
        DbCmd::Encrypt { kek } => encrypt(ctx, kek.as_deref()),
        DbCmd::Rekey { kek } => rekey(ctx, kek.as_deref()),
        DbCmd::Unlock => {
            let env = sealed_envelope(ctx)?;
            env.cache_session()?;
            ctx.info("database key cached in the OS keyring until `genome db lock`");
            ctx.audit("db unlock", json!({}))?;
            status(ctx)
        }
        DbCmd::Lock => lock(ctx),
        DbCmd::Status => status(ctx),
    }
}

fn pref<'a>(ctx: &'a Ctx, flag: Option<&'a str>) -> &'a str {
    flag.unwrap_or_else(|| ctx.get("kek"))
}

fn sealed_envelope(ctx: &Ctx) -> Result<Envelope> {
    match db::inspect(&ctx.db_path) {
        DbFile::Sealed => db::read_envelope(&ctx.db_path),
        _ => Err(AppError::usage(format!("{} is not an encrypted database", ctx.db_path.display()))),
    }
}

fn init(ctx: &Ctx, kek: Option<&str>) -> Result<()> {
    if db::inspect(&ctx.db_path) != DbFile::Missing {
        return Err(AppError::usage(format!("{} already exists (see `genome db status`)", ctx.db_path.display())));
    }
    if ctx.insecure_plaintext() {
        drop(ctx.db()?);
        ctx.info(&format!("created UNENCRYPTED database {}", ctx.db_path.display()));
    } else {
        let (envelope, dek) = Envelope::create(pref(ctx, kek), &Tty)?;
        drop(Db::open_sealed(&ctx.db_path, envelope.clone(), dek)?);
        crate::setup::announce(ctx, &envelope);
        ctx.info(&format!(
            "created encrypted database {} (XChaCha20-Poly1305, keys: {})",
            ctx.db_path.display(),
            envelope.kinds()
        ));
    }
    ctx.audit("db init", json!({"encrypted": !ctx.insecure_plaintext()}))?;
    status(ctx)
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

/// Seal one plaintext store file in place: seal to a staging file, verify the
/// round trip, shred the plaintext, rename. Returns true when it was sealed.
fn seal_in_place(p: &Path, key: &Key, label: &str) -> Result<bool> {
    if !p.exists() || crypto::is_sealed_file(p) {
        return Ok(false);
    }
    let staged = with_suffix(p, ".sealed-new");
    crypto::seal_file(p, &staged, key, label)?;
    let original = sha256(&std::fs::read(p)?);
    let back = SealedReader::open(&staged, key, label)?.read_all()?;
    if sha256(&back) != original {
        let _ = std::fs::remove_file(&staged);
        return Err(AppError::new(ErrorKind::Crypto, format!("{}: round-trip verification failed", p.display())));
    }
    crypto::shred(p)?;
    std::fs::rename(&staged, p)?;
    crypto::sync_parent(p);
    Ok(true)
}

/// Sidecars of a plaintext fsqlite database file: `-wal`, `-shm`, `-journal`
/// and fsqlite's own (`-fsqlite-ns-*`, `-wal-cert*`, `.fsqlite-*`).
fn sidecars(db_path: &Path) -> Vec<PathBuf> {
    let name = db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = db_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix(&name).is_some_and(|rest| rest.starts_with('-') || rest.starts_with(".fsqlite"))
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect()
}

fn encrypt(ctx: &Ctx, kek: Option<&str>) -> Result<()> {
    let path = &ctx.db_path;
    let (dek, kits, rows_migrated) = match db::inspect(path) {
        DbFile::Missing => {
            return Err(AppError::not_found(format!(
                "no database at {} (new databases are encrypted by default: `genome db init`)",
                path.display()
            )))
        }
        DbFile::Unknown => return Err(AppError::invalid(format!("{} is not a database", path.display()))),
        // Already encrypted: still seal any plaintext stores left by an interrupted migration.
        DbFile::Sealed => {
            let d = ctx.db()?;
            let kits = store::list(&d)?;
            (d.dek().cloned().expect("sealed"), kits, 0)
        }
        DbFile::Plaintext => {
            let staged = with_suffix(path, ".sealed-new");
            // Resume an interrupted migration with its key rather than minting a new one.
            let (envelope, dek) = if db::inspect(&staged) == DbFile::Sealed {
                let env = db::read_envelope(&staged)?;
                let dek = env.unlock()?;
                (env, dek)
            } else {
                let _ = std::fs::remove_file(&staged);
                let (env, dek) = Envelope::create(pref(ctx, kek), &Tty)?;
                crate::setup::announce(ctx, &env);
                (env, dek)
            };
            let plain = Db::open(path)?;
            let dump = plain.dump()?;
            let kits = store::list(&plain)?;
            drop(plain);
            {
                let sealed = Db::open_sealed(&staged, envelope.clone(), dek.clone())?;
                sealed.load_dump(&dump)?;
                sealed.persist()?;
            }
            let check = Db::open_sealed(&staged, envelope, dek.clone())?.dump()?;
            if check != dump {
                return Err(AppError::new(ErrorKind::Crypto, "encrypted database round-trip verification failed"));
            }
            let rows = dump["tables"]["kits"]["rows"].as_array().map_or(0, Vec::len);
            (dek, kits, rows)
        }
    };
    let mut files = 0;
    for k in &kits {
        let dir = Path::new(&k.store_dir);
        for f in gtstore::FILES {
            files += usize::from(seal_in_place(&dir.join(f), &dek, f)?);
        }
    }
    let audit_entries = crate::audit::migrate_plain(&ctx.data_dir, &dek)?;
    let staged = with_suffix(path, ".sealed-new");
    if staged.exists() {
        // Only now remove the plaintext database (overwrite + delete) and move the sealed one in.
        crypto::shred(path)?;
        for s in sidecars(path) {
            crypto::shred(&s)?;
        }
        std::fs::rename(&staged, path)?;
        crypto::sync_parent(path);
        let _ = std::fs::remove_file(with_suffix(&staged, ".lock"));
    }
    ctx.info(&format!(
        "encrypted {}: {rows_migrated} kits, {files} genotype store files, {audit_entries} audit entries",
        path.display()
    ));
    ctx.audit("db encrypt", json!({"kits": rows_migrated, "store_files": files, "audit_entries": audit_entries}))?;
    status(ctx)
}

fn rekey(ctx: &Ctx, kek: Option<&str>) -> Result<()> {
    sealed_envelope(ctx)?;
    let mut d = ctx.db()?;
    let old = d.envelope().cloned().expect("sealed");
    let dek = d.dek().cloned().expect("sealed");
    let new = old.rekey(&dek, pref(ctx, kek), &Tty)?;
    d.set_envelope(new.clone())?;
    drop(d);
    new.commit_keys()?;
    new.retire_unused(&old);
    crate::setup::record(ctx, &new);
    ctx.info(&format!("database key re-wrapped ({} -> {})", old.kinds(), new.kinds()));
    ctx.audit("db rekey", json!({"from": old.kinds(), "to": new.kinds()}))?;
    status(ctx)
}

fn stale_tmp(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            stale_tmp(&p, out);
        } else if name
            .rsplit_once(".tmp")
            .is_some_and(|(_, pid)| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
            || name.ends_with(".sealed-new")
        {
            out.push(p);
        }
    }
}

fn lock(ctx: &Ctx) -> Result<()> {
    let mut stale = Vec::new();
    stale_tmp(&ctx.data_dir, &mut stale);
    for p in &stale {
        crypto::shred(p)?;
    }
    let session = match db::inspect(&ctx.db_path) {
        DbFile::Sealed => db::read_envelope(&ctx.db_path)?.clear_session().unwrap_or(false),
        _ => false,
    };
    ctx.info(&format!(
        "locked: {} cached key removed; {} stale temporary files shredded",
        if session { "1" } else { "no" },
        stale.len()
    ));
    status(ctx)
}

/// Count sealed and plaintext genotype store files (by magic; no key needed).
pub fn store_files(kits_dir: &Path) -> (usize, usize) {
    let (mut sealed, mut plain) = (0, 0);
    if let Ok(rd) = std::fs::read_dir(kits_dir) {
        for kit in rd.flatten() {
            for f in gtstore::FILES {
                let p = kit.path().join(f);
                if p.exists() {
                    if crypto::is_sealed_file(&p) {
                        sealed += 1;
                    } else {
                        plain += 1;
                    }
                }
            }
        }
    }
    (sealed, plain)
}

/// Encryption status rows (no key needed; used by `db status` and `doctor`).
pub fn status_rows(ctx: &Ctx) -> (Vec<Value>, Vec<String>) {
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    let state = db::inspect(&ctx.db_path);
    let st = match state {
        DbFile::Missing => "missing",
        DbFile::Plaintext => "PLAINTEXT",
        DbFile::Sealed => "encrypted",
        DbFile::Unknown => "unknown",
    };
    if state == DbFile::Plaintext {
        warnings.push(format!("{} is not encrypted: run `genome db encrypt`", ctx.db_path.display()));
    }
    rows.push(json!({"check": "encryption", "status": st, "detail": ctx.db_path,
        "purpose": "kit database (sealed container, in-memory fsqlite)", "hint": (state == DbFile::Plaintext).then_some("genome db encrypt")}));
    if state == DbFile::Sealed {
        match db::read_envelope(&ctx.db_path) {
            Ok(e) => {
                let (slot_rows, slot_warnings) = crate::commands::key_cmd::slot_rows(&e);
                rows.extend(slot_rows);
                warnings.extend(slot_warnings);
            }
            Err(err) => warnings.push(err.message),
        }
    }
    let (sealed, plain) = store_files(&ctx.kits_dir());
    if plain > 0 {
        warnings.push(format!("{plain} genotype store files are plaintext"));
    }
    rows.push(json!({"check": "genotype stores", "status": if plain == 0 { "ok" } else { "PLAINTEXT" },
        "detail": format!("{sealed} sealed, {plain} plaintext files"), "purpose": "per-kit chunked AEAD stores",
        "hint": (plain > 0).then_some("genome db encrypt")}));
    let (ap, ak) = (crate::audit::sealed_path(&ctx.data_dir), crate::audit::plain_path(&ctx.data_dir));
    rows.push(json!({"check": "audit log",
        "status": if ak.exists() { "PLAINTEXT" } else if ap.exists() { "ok" } else { "empty" },
        "detail": if ak.exists() { ak } else { ap }, "purpose": "append-only hash-chained audit trail", "hint": null}));
    (rows, warnings)
}

fn status(ctx: &Ctx) -> Result<()> {
    let (rows, mut warnings) = status_rows(ctx);
    if ctx.insecure_plaintext() && db::inspect(&ctx.db_path) == DbFile::Plaintext {
        warnings.push(PLAINTEXT_WARNING.trim_start_matches("genome: warning: ").to_string());
    }
    let rows = rows.iter().map(to_record).collect();
    ctx.emit(&Report::new("db-status", rows).table_columns(&["check", "status", "detail"]).warnings(warnings))
}

pub fn audit(ctx: &Ctx, cmd: AuditCmd) -> Result<()> {
    let AuditCmd::Log { limit } = cmd;
    let key = ctx.store_key()?;
    let entries = crate::audit::read(&ctx.data_dir, key.as_ref())?;
    let skip = limit.map_or(0, |n| entries.len().saturating_sub(n));
    let rows = entries
        .into_iter()
        .skip(skip)
        .map(|mut e| {
            if let Some(d) = e.get_mut("details") {
                *d = Value::String(d.to_string());
            }
            to_record(&e)
        })
        .collect();
    ctx.emit(&Report::new("audit", rows).table_columns(&["seq", "ts", "command", "details"]))
}

pub fn decrypt(ctx: &Ctx, a: DecryptArgs) -> Result<()> {
    let bytes = std::fs::read(&a.file).map_err(|e| AppError::io(format!("{}: {e}", a.file.display())))?;
    let plain = if bytes.starts_with(crate::keys::EXPORT_MAGIC) {
        crate::keys::open_export(&bytes)?
    } else if bytes.starts_with(crypto::SEAL_MAGIC) {
        let key = ctx.store_key()?.ok_or_else(|| AppError::usage("sealed files need the encrypted database's key"))?;
        SealedReader::open(&a.file, &key, crate::pipeline::SEALED_OUTPUT_LABEL)?.read_all()?
    } else {
        return Err(AppError::invalid(format!("{} is not a genome-cli encrypted file", a.file.display())));
    };
    ctx.audit("decrypt", json!({"bytes": plain.len()}))?;
    let mut c = ctx.clone();
    c.encrypt_output = false;
    c.write_output(&plain, true)
}
