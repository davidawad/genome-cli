//! Key slots end to end: first run with the user's SSH key (no setup), key
//! files for passphrase-protected SSH keys, recovery, `genome key ...`, and
//! migrating a 0.2 OS-keyring database. Throwaway SSH keys are generated
//! in-process; the real ~/.ssh and the real OS keyring are never touched
//! (GENOME_SSH_DIR, GENOME_KEY_DIR, GENOME_NO_KEYRING, GENOME_TEST_KEYSTORE_DIR).

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use genome_cli::platform::perms::{check_private, Access};
use serde_json::Value;
use ssh_key::{rand_core::OsRng, Algorithm, LineEnding, PrivateKey};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

/// Write an ed25519 key pair `dir/name` + `dir/name.pub`; returns the private key path.
fn ssh_key(dir: &Path, name: &str, passphrase: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut k = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    k.set_comment(format!("{name}@test"));
    let public = k.public_key().to_openssh().unwrap();
    if let Some(p) = passphrase {
        k = k.encrypt(&mut OsRng, p).unwrap();
    }
    let path = dir.join(name);
    std::fs::write(&path, k.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    std::fs::write(dir.join(format!("{name}.pub")), format!("{public}\n")).unwrap();
    path
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        Self { dir: TempDir::new().unwrap() }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ssh_dir(&self) -> PathBuf {
        self.path("ssh")
    }

    fn config(&self) -> PathBuf {
        self.path("config").join("genome-cli").join("config.toml")
    }

    /// No GENOME_KEY: the first-run default (SSH key, else key file) applies.
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("genome").unwrap();
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("GENOME_DATA_DIR", self.path("data"))
            .env("GENOME_CACHE_DIR", self.path("cache"))
            .env("GENOME_KEY_DIR", self.path("keys"))
            .env("GENOME_SSH_DIR", self.ssh_dir())
            .env("GENOME_NO_KEYRING", "1")
            .env("GENOME_OFFLINE", "1")
            .env("GENOME_INSECURE_FAST_KDF", "1")
            .env("NO_COLOR", "1");
        for v in ["SystemRoot", "SYSTEMROOT", "windir", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP"] {
            if let Some(val) = std::env::var_os(v) {
                c.env(v, val);
            }
        }
        c
    }

    fn run_with(&self, c: &mut Command, args: &[&str]) -> (String, String) {
        let out = c.args(args).output().unwrap();
        let (o, e) =
            (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        assert!(out.status.success(), "genome {args:?} failed ({:?}):\n{o}\n{e}", out.status.code());
        (o, e)
    }

    fn run(&self, args: &[&str]) -> (String, String) {
        self.run_with(&mut self.cmd(), args)
    }

    fn fail(&self, c: &mut Command, args: &[&str]) -> (i32, String) {
        let out = c.args(args).output().unwrap();
        assert!(!out.status.success(), "genome {args:?} unexpectedly succeeded");
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn import(&self) -> String {
        let f = fixture("23andme_male.txt");
        self.run(&["import", f.to_str().unwrap(), "--name", "kit"]).1
    }

    fn key_status(&self, c: &mut Command) -> Vec<Value> {
        let (o, _) = self.run_with(c, &["key", "status", "--format", "json"]);
        let v: Value = serde_json::from_str(&o).unwrap();
        v["data"].as_array().unwrap().clone()
    }

    fn slot_ids(&self, kind: &str) -> Vec<String> {
        self.key_status(&mut self.cmd())
            .iter()
            .filter(|r| r["check"].as_str().is_some_and(|c| c.starts_with(&format!("key {kind}-"))))
            .map(|r| r["check"].as_str().unwrap().trim_start_matches("key ").to_string())
            .collect()
    }

    fn key_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.path("keys"))
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "key")).collect())
            .unwrap_or_default()
    }
}

#[test]
fn first_run_encrypts_with_the_ssh_key_and_explains_itself() {
    let e = Env::new();
    ssh_key(&e.ssh_dir(), "id_ed25519", None);
    let notice = e.import();
    assert!(notice.contains("SSH key") && notice.contains("SHA256:"), "{notice}");
    assert!(notice.contains("[encryption]") && notice.contains("config.toml"), "{notice}");
    // The CLI's own config file says which key decrypts and how to recover.
    let cfg = std::fs::read_to_string(e.config()).unwrap();
    let t: toml::Table = cfg.parse().unwrap();
    let enc = t["encryption"].as_table().unwrap();
    assert_eq!(enc["ssh_fingerprints"].as_array().unwrap().len(), 1);
    assert!(cfg.contains("genome doctor") && cfg.contains("cannot be recovered"));
    // Later commands are silent and need nothing.
    let (out, err) = e.run(&["kits"]);
    assert!(out.contains("kit") && !err.contains("created an encrypted database"), "{err}");
    assert!(e.key_files().is_empty(), "an unencrypted SSH key needs no key file");
    // Without the SSH key the data stays locked.
    std::fs::rename(e.ssh_dir().join("id_ed25519"), e.path("moved")).unwrap();
    let (code, err) = e.fail(&mut e.cmd(), &["kits"]);
    assert_eq!(code, 10, "{err}");
    assert!(err.contains("SSH private key") && err.contains("GENOME_SSH_KEY"), "{err}");
    // GENOME_SSH_KEY points at it wherever it is (another machine's layout).
    e.run_with(e.cmd().env("GENOME_SSH_KEY", e.path("moved")), &["kits"]);
    // `config set` keeps the managed section.
    e.run(&["config", "set", "format", "json"]);
    assert!(std::fs::read_to_string(e.config()).unwrap().contains("[encryption]"));
}

#[test]
fn passphrase_protected_ssh_key_gets_a_key_file_and_stays_the_recovery_key() {
    let e = Env::new();
    ssh_key(&e.ssh_dir(), "id_ed25519", Some("ssh-secret"));
    let notice = e.import();
    assert!(notice.contains("recovery key"), "{notice}");
    let files = e.key_files();
    assert_eq!(files.len(), 1);
    assert!(matches!(check_private(&files[0]), Access::Private(_)), "{:?}", check_private(&files[0]));
    assert!(matches!(check_private(&e.path("keys")), Access::Private(_)));
    e.run(&["kits"]); // the key file: no passphrase asked
    std::fs::remove_file(&files[0]).unwrap();
    let (code, err) = e.fail(&mut e.cmd(), &["kits"]);
    assert_eq!(code, 10, "{err}");
    assert!(err.contains("GENOME_SSH_PASSPHRASE"), "{err}");
    e.run_with(e.cmd().env("GENOME_SSH_PASSPHRASE", "ssh-secret"), &["kits"]);
}

#[test]
fn no_ssh_key_means_a_private_key_file() {
    let e = Env::new();
    let notice = e.import();
    assert!(notice.contains("key file"), "{notice}");
    let files = e.key_files();
    assert_eq!(files.len(), 1);
    assert!(!files[0].starts_with(e.path("data")), "the key never sits in the data dir");
    assert!(matches!(check_private(&files[0]), Access::Private(_)));
    let status = e.key_status(&mut e.cmd());
    assert_eq!(status[0]["status"], "file");
    let (code, err) = e.fail(e.cmd().env("GENOME_KEY_DIR", e.path("elsewhere")), &["kits"]);
    assert_eq!(code, 10);
    assert!(err.contains("is missing"), "{err}");
}

#[test]
fn key_add_and_remove() {
    let e = Env::new();
    ssh_key(&e.ssh_dir(), "id_ed25519", None);
    e.import();
    let laptop = ssh_key(&e.path("laptop"), "id_ed25519", None);
    let laptop_pub = e.path("laptop").join("id_ed25519.pub");
    e.run(&["key", "add-ssh", laptop_pub.to_str().unwrap()]);
    e.fail(&mut e.cmd(), &["key", "add-ssh", laptop_pub.to_str().unwrap()]);
    e.run_with(e.cmd().env("GENOME_NEW_KEY", "a passphrase"), &["key", "add-passphrase"]);
    assert_eq!(e.slot_ids("ssh").len(), 2);
    // Drop the original SSH key's slot; the laptop key and the passphrase still work.
    let original = e
        .key_status(&mut e.cmd())
        .iter()
        .find(|r| r["detail"].as_str().is_some_and(|d| d.contains(e.ssh_dir().to_str().unwrap())))
        .map(|r| r["check"].as_str().unwrap().trim_start_matches("key ").to_string())
        .unwrap();
    e.run(&["key", "remove", &original]);
    e.run_with(e.cmd().env("GENOME_SSH_KEY", &laptop), &["kits"]);
    e.run_with(e.cmd().env("GENOME_SSH_DIR", e.path("none")).env("GENOME_KEY", "a passphrase"), &["kits"]);
    e.run(&["key", "remove", "passphrase"]);
    let (code, err) = e.fail(e.cmd().env("GENOME_SSH_KEY", &laptop), &["key", "remove", "ssh"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("last key slot"), "{err}");
    let cfg = std::fs::read_to_string(e.config()).unwrap();
    assert_eq!(cfg.matches("SHA256:").count(), 1, "config lists the remaining key only:\n{cfg}");
}

#[test]
fn legacy_keyring_database_migrates_to_the_ssh_key() {
    let e = Env::new();
    let store = e.path("keystore");
    let keyring = |c: &mut Command| {
        c.env_remove("GENOME_NO_KEYRING").env("GENOME_TEST_KEYSTORE_DIR", &store);
    };
    let mut c = e.cmd();
    keyring(&mut c);
    e.run_with(&mut c, &["db", "init", "--kek", "keyring"]);
    assert_eq!(std::fs::read_dir(&store).unwrap().count(), 1);
    ssh_key(&e.ssh_dir(), "id_ed25519", None);
    let mut c = e.cmd();
    keyring(&mut c);
    let (_, err) = e.run_with(&mut c, &["db", "rekey", "--to", "ssh"]);
    assert!(err.contains("keyring -> ssh"), "{err}");
    assert_eq!(std::fs::read_dir(&store).unwrap().count(), 0, "the keyring entry is deleted");
    e.run(&["kits"]); // no keyring needed any more
    assert!(std::fs::read_to_string(e.config()).unwrap().contains("legacy_keyring = false"));
}

#[test]
fn passphrase_databases_still_work_without_any_ssh_key() {
    let e = Env::new();
    let mut c = e.cmd();
    c.env("GENOME_KEY", "pw-only");
    let f = fixture("23andme_male.txt");
    let (_, err) = e.run_with(&mut c, &["import", f.to_str().unwrap(), "--name", "kit"]);
    assert!(!err.contains("created an encrypted database"), "GENOME_KEY users chose their key: {err}");
    e.run_with(e.cmd().env("GENOME_KEY", "pw-only"), &["kits"]);
    let (code, err) = e.fail(e.cmd().env("GENOME_KEY", "wrong"), &["kits"]);
    assert_eq!(code, 10);
    assert!(err.contains("wrong key"), "{err}");
}
