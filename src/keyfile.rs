//! Key files: the default KEK source, kept like an ssh private key.
//!
//! A random 256-bit KEK is written as one hex line to
//! `<key dir>/<db-id>.key` (see [`crate::platform::dirs::key_dir`]): owner-only
//! (0600 in a 0700 directory; a protected DACL on Windows) and outside the data
//! directory, so a copied, synced or backed-up data directory never carries its
//! own key. `GENOME_KEY_FILE` names the file explicitly (it is then shared by
//! every database created with it, and recorded in the envelope).
//!
//! Like ssh, a key file other users can read is refused.
//!
//! Rotation (`db rekey`) writes the new key to `<file>.new` and only renames it
//! over the old one once the database holds the new envelope ([`commit`]);
//! [`load_candidates`] also returns the staged key, so a crash in between
//! leaves the database readable.

use std::io::Write;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::crypto::{hex, unhex, Key};
use crate::error::{AppError, ErrorKind, Result};
use crate::platform::dirs;
use crate::platform::perms::{self, Access};

pub const KEY_FILE_ENV: &str = "GENOME_KEY_FILE";

fn err(m: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Crypto, m)
}

/// `GENOME_KEY_FILE`, when set and non-empty.
pub fn env_override() -> Option<PathBuf> {
    std::env::var_os(KEY_FILE_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The default key file of database `db_id`.
pub fn default_path(db_id: &str) -> PathBuf {
    dirs::key_dir().join(format!("{db_id}.key"))
}

/// The key file to use: `GENOME_KEY_FILE`, else the one recorded in the
/// envelope, else the default for `db_id`.
pub fn resolve(db_id: &str, recorded: Option<&str>) -> PathBuf {
    env_override().or_else(|| recorded.map(PathBuf::from)).unwrap_or_else(|| default_path(db_id))
}

fn staged(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".new");
    path.with_file_name(name)
}

/// Refuse a key file other users can read (like ssh).
fn ensure_private(path: &Path) -> Result<()> {
    match perms::check_private(path) {
        Access::Open(why) => Err(err(format!(
            "key file {} is accessible to other users ({why}); it is refused. {}",
            path.display(),
            fix_hint(path)
        ))),
        _ => Ok(()),
    }
}

pub fn fix_hint(path: &Path) -> String {
    if cfg!(windows) {
        format!("Restrict it to your account (icacls \"{}\" /inheritance:r /grant:r %USERNAME%:F)", path.display())
    } else {
        format!("Run: chmod 600 {}", path.display())
    }
}

/// Read and validate a key file.
pub fn read(path: &Path) -> Result<Key> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => Zeroizing::new(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(err(format!(
                "key file {} is missing: restore it from your backup, or point {KEY_FILE_ENV} at it",
                path.display()
            )))
        }
        Err(e) => return Err(err(format!("reading key file {}: {e}", path.display()))),
    };
    ensure_private(path)?;
    let bytes = Zeroizing::new(
        unhex(text.trim()).map_err(|_| err(format!("{} is not a genome-cli key file", path.display())))?,
    );
    Key::from_bytes(&bytes).map_err(|_| err(format!("{} is not a genome-cli key file", path.display())))
}

/// The key in `path`, then a staged rotation (`<path>.new`) if one exists.
pub fn load_candidates(path: &Path) -> Result<Vec<Key>> {
    let mut keys = vec![read(path)?];
    let next = staged(path);
    if next.exists() {
        keys.push(read(&next)?);
    }
    Ok(keys)
}

fn write_new(path: &Path, key: &Key) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if parent == dirs::key_dir() {
        perms::private_dir(parent)?;
    } else {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let mut f = perms::private_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| err(format!("creating key file {}: {e}", path.display())))?;
    perms::restrict_file(path)?;
    let line = Zeroizing::new(format!("{}\n", hex(key.bytes())));
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    crate::crypto::sync_parent(path);
    Ok(())
}

/// A KEK for database `db_id` and the path to record in its envelope (only
/// for an explicit `GENOME_KEY_FILE`). An existing explicit key file is
/// reused; an existing default one is rotated through `<file>.new`.
pub fn create(db_id: &str) -> Result<(Key, Option<String>)> {
    if let Some(p) = env_override() {
        if p.exists() {
            return Ok((read(&p)?, Some(p.display().to_string())));
        }
        let key = Key::random()?;
        write_new(&p, &key)?;
        return Ok((key, Some(p.display().to_string())));
    }
    let path = default_path(db_id);
    let key = Key::random()?;
    let target = if path.exists() { staged(&path) } else { path };
    write_new(&target, &key)?;
    Ok((key, None))
}

/// Finish a rotation: move a staged `<file>.new` over the key file.
pub fn commit(db_id: &str, recorded: Option<&str>) -> Result<()> {
    let path = resolve(db_id, recorded);
    let next = staged(&path);
    if next.exists() {
        std::fs::rename(&next, &path)?;
        crate::crypto::sync_parent(&path);
    }
    Ok(())
}

/// Remove a key file no longer used by its database (after a rekey away from
/// `file`). Explicit `GENOME_KEY_FILE` files may be shared and are kept.
pub fn retire(db_id: &str, recorded: Option<&str>) -> Result<bool> {
    if recorded.is_some() {
        return Ok(false);
    }
    let path = default_path(db_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(err(format!("removing {}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rotation_staging() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("k.key");
        let a = Key::random().unwrap();
        write_new(&p, &a).unwrap();
        assert_eq!(read(&p).unwrap().bytes(), a.bytes());
        let b = Key::random().unwrap();
        write_new(&staged(&p), &b).unwrap();
        let ks = load_candidates(&p).unwrap();
        assert_eq!((ks[0].bytes(), ks[1].bytes()), (a.bytes(), b.bytes()));
    }

    #[test]
    fn shared_key_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("k.key");
        write_new(&p, &Key::random().unwrap()).unwrap();
        assert!(matches!(perms::check_private(&p), Access::Private(_)), "{:?}", perms::check_private(&p));
        perms::share_for_test(&p).unwrap();
        assert!(matches!(perms::check_private(&p), Access::Open(_)), "{:?}", perms::check_private(&p));
        let e = read(&p).err().unwrap();
        assert!(e.message.contains("accessible to other users"), "{}", e.message);
        assert!(e.message.contains(&fix_hint(&p)), "{}", e.message);
    }

    #[test]
    fn missing_and_garbage_key_files_fail_clearly() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("k.key");
        assert!(read(&p).err().unwrap().message.contains("is missing"));
        write_new(&p, &Key::random().unwrap()).unwrap();
        std::fs::write(&p, "nope\n").unwrap();
        perms::restrict_file(&p).unwrap();
        assert!(read(&p).err().unwrap().message.contains("not a genome-cli key file"));
    }
}
