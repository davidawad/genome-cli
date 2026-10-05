//! What the user is told about encryption, and the `[encryption]` section the
//! CLI keeps in its own config file so it is always clear which keys decrypt
//! the data and how to get it back on another machine.

use std::path::Path;

use crate::context::Ctx;
use crate::keys::{Envelope, Slot, SlotKind};

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| q(s)).collect::<Vec<_>>().join(", "))
}

fn of_kind(env: &Envelope, kind: SlotKind) -> impl Iterator<Item = &Slot> {
    env.slots.iter().filter(move |s| s.kind == kind)
}

fn ssh_identity(s: &Slot) -> String {
    s.ssh_identity.clone().unwrap_or_else(|| "the matching SSH private key".into())
}

/// Plain-English recovery steps for this envelope.
pub fn recovery_steps(env: &Envelope, data_dir: &Path) -> Vec<String> {
    let mut keys: Vec<String> = of_kind(env, SlotKind::Ssh)
        .map(|s| format!("your SSH private key {} ({})", ssh_identity(s), s.ssh_fingerprint.as_deref().unwrap_or("?")))
        .collect();
    keys.extend(of_kind(env, SlotKind::File).map(|s| format!("the key file {}", env.key_file_path(s).display())));
    keys.extend(of_kind(env, SlotKind::Passphrase).map(|_| "your database passphrase (GENOME_KEY)".to_string()));
    let mut place = Vec::new();
    if env.has(SlotKind::Ssh) {
        place.push("put the SSH key in ~/.ssh (or point GENOME_SSH_KEY at it)");
    }
    if env.has(SlotKind::File) {
        place.push("put the key file at the same path (or point GENOME_KEY_FILE at it)");
    }
    if env.has(SlotKind::Passphrase) {
        place.push("set GENOME_KEY or type the passphrase when asked");
    }
    vec![
        format!("copy the data directory {}", data_dir.display()),
        format!("bring ANY ONE of: {}", keys.join("; or ")),
        place.join("; "),
        "run `genome doctor`: it reports which key unlocked the data".into(),
    ]
}

/// The managed `[encryption]` section (comments included).
pub fn encryption_block(env: &Envelope, data_dir: &Path, db_path: &Path) -> String {
    let fps: Vec<String> = of_kind(env, SlotKind::Ssh).filter_map(|s| s.ssh_fingerprint.clone()).collect();
    let ids: Vec<String> = of_kind(env, SlotKind::Ssh).filter_map(|s| s.ssh_identity.clone()).collect();
    let pubs: Vec<String> = of_kind(env, SlotKind::Ssh).filter_map(|s| s.ssh_public_key.clone()).collect();
    let files: Vec<String> = of_kind(env, SlotKind::File).map(|s| env.key_file_path(s).display().to_string()).collect();
    let slots: Vec<String> = env.slots.iter().map(|s| s.id.clone()).collect();
    let mut out = format!("{}\n", crate::config::MANAGED_MARK);
    out.push_str("# Your kit database and genotype stores are encrypted at rest (XChaCha20-Poly1305).\n");
    out.push_str("# Any ONE of the keys listed below decrypts them. genome-cli rewrites this section\n");
    out.push_str("# whenever keys change (`genome key status` shows the same).\n#\n# To recover on another machine:\n");
    for (i, step) in recovery_steps(env, data_dir).iter().enumerate() {
        out.push_str(&format!("#   {}. {step}\n", i + 1));
    }
    out.push_str("# If every key below is lost, the data cannot be recovered: back one of them up.\n");
    out.push_str("[encryption]\n");
    out.push_str(&format!(
        "data_dir = {}\ndatabase = {}\ndb_id = {}\n",
        q(&data_dir.display().to_string()),
        q(&db_path.display().to_string()),
        q(&env.db_id)
    ));
    out.push_str(&format!(
        "slots = {}\nssh_fingerprints = {}\nssh_identities = {}\n",
        list(&slots),
        list(&fps),
        list(&ids)
    ));
    out.push_str(&format!("ssh_public_keys = {}\nkey_files = {}\n", list(&pubs), list(&files)));
    out.push_str(&format!("passphrase = {}\n", env.has(SlotKind::Passphrase)));
    out
}

/// Keep the config file's `[encryption]` section in step with the envelope
/// (best effort: a read-only config never blocks access to the data).
pub fn record(ctx: &Ctx, env: &Envelope) {
    let block = encryption_block(env, &ctx.data_dir, &ctx.db_path);
    if let Err(e) = crate::config::write_managed_block(&ctx.resolved.config_path, &block) {
        ctx.info(&format!("genome: warning: could not update {}: {}", ctx.resolved.config_path.display(), e.message));
    }
}

/// Lines telling the user how a new database is protected.
pub fn notice(env: &Envelope, db_path: &Path, config_path: &Path) -> Vec<String> {
    let mut lines = vec![format!("genome: created an encrypted database at {}", db_path.display())];
    for s in of_kind(env, SlotKind::Ssh) {
        lines.push(format!("  key: SSH key {} ({})", ssh_identity(s), s.ssh_fingerprint.as_deref().unwrap_or("?")));
    }
    for s in of_kind(env, SlotKind::File) {
        let why = if env.has(SlotKind::Ssh) {
            " (your SSH key has a passphrase: this key file unlocks everyday use without asking; the SSH key is your recovery key)"
        } else {
            ""
        };
        lines.push(format!("  key: key file {}{why}", env.key_file_path(s).display()));
    }
    lines.push(format!(
        "  how to decrypt or recover it later: see the [encryption] section of {}",
        config_path.display()
    ));
    lines.push("  back up one of these keys: if all are lost, the data cannot be recovered".into());
    lines
}

/// Tell the user about a database we just created (not for passphrase-only
/// databases: whoever set GENOME_KEY chose that key on purpose).
pub fn announce(ctx: &Ctx, env: &Envelope) {
    record(ctx, env);
    if env.slots.iter().all(|s| s.kind == SlotKind::Passphrase) {
        return;
    }
    for line in notice(env, &ctx.db_path, &ctx.resolved.config_path) {
        ctx.info(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testenv::sandbox;
    use crate::prompt::Scripted;
    use crate::sshkey::testkeys;

    #[test]
    fn block_and_notice_name_every_key_and_parse_as_toml() {
        let sb = sandbox();
        testkeys::ed25519(&sb.ssh, "id_ed25519", Some("pw"));
        let (env, _) = Envelope::create("auto", &Scripted::new(true, &["y"])).unwrap();
        let data = sb.dir.path().join("data");
        let block = encryption_block(&env, &data, &data.join("genome.db"));
        let t: toml::Table = block.parse().unwrap();
        let enc = t["encryption"].as_table().unwrap();
        assert_eq!(enc["ssh_fingerprints"].as_array().unwrap().len(), 1);
        assert_eq!(enc["key_files"].as_array().unwrap().len(), 1);
        assert_eq!(enc["data_dir"].as_str().unwrap(), data.display().to_string());
        assert!(block.contains("genome doctor") && block.contains("cannot be recovered"));
        let n = notice(&env, &data.join("genome.db"), &sb.dir.path().join("config.toml")).join("\n");
        assert!(n.contains("SHA256:") && n.contains("recovery key") && n.contains("[encryption]"), "{n}");
    }
}
