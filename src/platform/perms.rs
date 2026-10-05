//! Owner-only access to the data directory and the files in it.
//!
//! Unix: directories 0700, files 0600. Windows: the directory gets a protected
//! DACL granting only the current user full control, inherited by everything
//! created inside it (files need no separate step).

use std::fs::{File, OpenOptions};
use std::path::Path;

/// Create DIR if missing and make it owner-only, so the sealed files inside
/// are not even listable by other local users.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir)
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn restrict_dir(dir: &Path) -> std::io::Result<()> {
    super::win_acl::protect(dir)
}

#[cfg(not(any(unix, windows)))]
fn restrict_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Make an existing file owner-only: 0600 on Unix; on Windows a protected,
/// non-inheritable DACL for the current user (for files that may live outside
/// a protected directory, such as key files).
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    restrict_file_imp(path)
}

#[cfg(unix)]
fn restrict_file_imp(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn restrict_file_imp(path: &Path) -> std::io::Result<()> {
    super::win_acl::protect_file(path)
}

#[cfg(not(any(unix, windows)))]
fn restrict_file_imp(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Test helper: make `path` readable by other users (0644 / an Everyone ACE).
#[cfg(test)]
pub fn share_for_test(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
    }
    #[cfg(windows)]
    {
        super::win_acl::grant_everyone_read(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// Options for creating a file readable only by its owner (0600 on Unix;
/// Windows files inherit the directory's DACL).
pub fn private_options() -> OpenOptions {
    let mut o = OpenOptions::new();
    owner_only(&mut o);
    o
}

#[cfg(unix)]
fn owner_only(o: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    o.mode(0o600);
}

#[cfg(not(unix))]
fn owner_only(_o: &mut OpenOptions) {}

/// `File::create` with owner-only permissions.
pub fn create_private(path: &Path) -> std::io::Result<File> {
    private_options().write(true).create(true).truncate(true).open(path)
}

/// Result of checking that a directory is owner-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Only the owner (Windows: plus SYSTEM / Administrators) can access it.
    Private(String),
    /// Others can access it; the string says how.
    Open(String),
    Missing,
    Unknown(String),
}

impl Access {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Private(_) => "ok",
            Self::Open(_) => "open",
            Self::Missing => "missing",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Private(d) | Self::Open(d) | Self::Unknown(d) => d.clone(),
            Self::Missing => "not created yet".into(),
        }
    }
}

pub fn check_private(dir: &Path) -> Access {
    match std::fs::metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Access::Missing,
        Err(e) => Access::Unknown(e.to_string()),
        Ok(_) => check(dir),
    }
}

#[cfg(unix)]
fn check(dir: &Path) -> Access {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(m) => {
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 == 0 {
                Access::Private(format!("mode {mode:04o}"))
            } else {
                Access::Open(format!("mode {mode:04o}: group/other can access"))
            }
        }
        Err(e) => Access::Unknown(e.to_string()),
    }
}

#[cfg(windows)]
fn check(dir: &Path) -> Access {
    super::win_acl::check(dir)
}

#[cfg(not(any(unix, windows)))]
fn check(_dir: &Path) -> Access {
    Access::Unknown("permission checks are not implemented on this OS".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_is_owner_only() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("data");
        assert_eq!(check_private(&d), Access::Missing);
        private_dir(&d).unwrap();
        let a = check_private(&d);
        assert_eq!(a.status(), "ok", "{a:?}");
        let f = d.join("f");
        drop(create_private(&f).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(check_private(&d).status(), "open");
        }
    }
}
