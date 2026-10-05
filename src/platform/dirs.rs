//! Default config, data and cache locations.
//!
//! | | Linux | macOS | Windows |
//! |---|---|---|---|
//! | config | `~/.config/genome-cli` | `~/Library/Application Support/genome-cli` | `%APPDATA%\genome-cli` |
//! | data | `~/.local/share/genome-cli` | `~/Library/Application Support/genome-cli` | `%LOCALAPPDATA%\genome-cli\data` |
//! | cache | `~/.cache/genome-cli` | `~/Library/Caches/genome-cli` | `%LOCALAPPDATA%\genome-cli\cache` |
//!
//! `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` (absolute paths)
//! override the OS convention everywhere. Releases up to 0.1 used the
//! XDG-style layout on macOS too: when the native macOS location holds no
//! database (`genome.db`) but the old one does, the old one is used (see
//! [`Dir::legacy`]). On macOS the native config and data directories are the
//! same, so the test is the database file, never the directory: writing the
//! config file must not make an empty native location win.

use std::path::{Path, PathBuf};

use directories::BaseDirs;

pub const APP: &str = "genome-cli";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Config,
    Data,
    Cache,
}

impl Dir {
    fn xdg_var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::Cache => "XDG_CACHE_HOME",
        }
    }

    fn xdg_fallback(self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::Cache => ".cache",
        }
    }

    /// `$XDG_*_HOME/genome-cli`, when the variable holds an absolute path.
    fn xdg_override(self) -> Option<PathBuf> {
        std::env::var_os(self.xdg_var()).map(PathBuf::from).filter(|p| p.is_absolute()).map(|p| p.join(APP))
    }

    /// The OS convention (see the module docs).
    pub fn native(self) -> PathBuf {
        let Some(b) = BaseDirs::new() else { return home().join(self.xdg_fallback()).join(APP) };
        if cfg!(windows) {
            return match self {
                Self::Config => b.config_dir().join(APP),
                Self::Data => b.data_local_dir().join(APP).join("data"),
                Self::Cache => b.cache_dir().join(APP).join("cache"),
            };
        }
        match self {
            Self::Config => b.config_dir().join(APP),
            Self::Data => b.data_dir().join(APP),
            Self::Cache => b.cache_dir().join(APP),
        }
    }

    /// The XDG-style location used on macOS up to 0.1 (`~/.local/share/genome-cli`, ...).
    /// `None` elsewhere: it is the native one on Linux and never shipped on Windows.
    pub fn legacy(self) -> Option<PathBuf> {
        cfg!(target_os = "macos").then(|| home().join(self.xdg_fallback()).join(APP))
    }

    /// The directory to use: XDG override, else the native location, else an
    /// existing legacy one when the native one is absent.
    pub fn resolve(self) -> PathBuf {
        if let Some(p) = self.xdg_override() {
            return p;
        }
        let native = self.native();
        let probe = |d: &Path| match self {
            Self::Config => d.join(CONFIG_FILE),
            Self::Data => d.join(DB_FILE),
            Self::Cache => d.to_path_buf(),
        };
        match self.legacy() {
            Some(old) => pick(native, old, probe),
            None => native,
        }
    }
}

pub const CONFIG_FILE: &str = "config.toml";
pub const DB_FILE: &str = "genome.db";

/// A pre-0.2 macOS store that a database at `data_dir` would shadow: set
/// when `data_dir` is the native location, holds no database, and the legacy
/// location does. A new database must never be created over it.
pub fn shadowed_legacy_store(data_dir: &Path) -> Option<PathBuf> {
    let legacy = Dir::Data.legacy()?;
    shadowed(data_dir, &Dir::Data.native(), &legacy)
}

fn shadowed(data_dir: &Path, native: &Path, legacy: &Path) -> Option<PathBuf> {
    let unused = data_dir == native && !native.join(DB_FILE).exists();
    (unused && legacy != native && legacy.join(DB_FILE).exists()).then(|| legacy.to_path_buf())
}

/// Where key files live: `$XDG_CONFIG_HOME/genome-cli/keys` when set, else
/// `~/.config/genome-cli/keys` on Unix (macOS included, so it is never the
/// data directory) and `%LOCALAPPDATA%\genome-cli\keys` on Windows (beside
/// `data`, not inside it). `GENOME_KEY_DIR` overrides it.
pub fn key_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("GENOME_KEY_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(p) = Dir::Config.xdg_override() {
        return p.join("keys");
    }
    if cfg!(windows) {
        if let Some(b) = BaseDirs::new() {
            return b.data_local_dir().join(APP).join("keys");
        }
    }
    home().join(".config").join(APP).join("keys")
}

/// Prefer `native` unless only `legacy` exists (as judged by `probe`).
fn pick(native: PathBuf, legacy: PathBuf, probe: impl Fn(&Path) -> PathBuf) -> PathBuf {
    if !probe(&native).exists() && probe(&legacy).exists() {
        legacy
    } else {
        native
    }
}

pub fn home() -> PathBuf {
    BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_only_wins_when_native_is_absent() {
        let t = tempfile::tempdir().unwrap();
        let (native, legacy) = (t.path().join("native"), t.path().join("legacy"));
        let id = |p: &Path| p.to_path_buf();
        assert_eq!(pick(native.clone(), legacy.clone(), id), native);
        std::fs::create_dir(&legacy).unwrap();
        assert_eq!(pick(native.clone(), legacy.clone(), id), legacy);
        std::fs::create_dir(&native).unwrap();
        assert_eq!(pick(native.clone(), legacy, id), native);
    }

    /// The macOS bug: the config file lives in the native data dir, so that
    /// directory exists; the legacy store must still win until a database exists natively.
    #[test]
    fn database_file_decides_not_the_directory() {
        let t = tempfile::tempdir().unwrap();
        let (native, legacy) = (t.path().join("native"), t.path().join("legacy"));
        let probe = |d: &Path| d.join(DB_FILE);
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join(CONFIG_FILE), "").unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(DB_FILE), "").unwrap();
        assert_eq!(pick(native.clone(), legacy.clone(), probe), legacy);
        assert_eq!(shadowed(&native, &native, &legacy), Some(legacy.clone()));
        assert_eq!(shadowed(&legacy, &native, &legacy), None, "using the legacy store is fine");
        assert_eq!(shadowed(&t.path().join("custom"), &native, &legacy), None, "explicit data dirs are left alone");
        std::fs::write(native.join(DB_FILE), "").unwrap();
        assert_eq!(pick(native.clone(), legacy.clone(), probe), native);
        assert_eq!(shadowed(&native, &native, &legacy), None);
    }

    #[test]
    fn native_layout_is_app_scoped() {
        for d in [Dir::Config, Dir::Data, Dir::Cache] {
            assert!(d.native().components().any(|c| c.as_os_str() == APP), "{:?}", d.native());
        }
        assert_eq!(Dir::Data.legacy().is_some(), cfg!(target_os = "macos"));
    }

    #[test]
    fn key_dir_is_outside_the_data_dir() {
        if std::env::var_os("GENOME_KEY_DIR").is_none() && std::env::var_os("XDG_CONFIG_HOME").is_none() {
            assert!(!key_dir().starts_with(Dir::Data.native()), "{:?}", key_dir());
        }
    }
}
