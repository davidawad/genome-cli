//! `genome doctor`: external tools, cache and data locations.

use std::process::Command as Proc;

use serde_json::json;

use crate::context::Ctx;
use crate::error::Result;
use crate::liftover::{chain_path, CHAINS};
use crate::model::Build;
use crate::output::{to_record, Record, Report};
use crate::pipeline::cached_reference_path;
use crate::platform::dirs::Dir;
use crate::platform::exe::{install_hint, which};
use crate::platform::keystore;
use crate::platform::perms::{check_private, Access};

/// (tool, version args, needed for)
const TOOLS: &[(&str, &[&str], &str)] = &[
    ("minimap2", &["--version"], "pipeline: alignment (default aligner)"),
    ("bwa-mem2", &["version"], "pipeline: alignment (--aligner bwa-mem2)"),
    ("samtools", &["--version"], "pipeline: faidx, fixmate, sort, merge, markdup, index"),
    ("bcftools", &["--version"], "pipeline: mpileup/call, filter, norm"),
    ("bgzip", &["--version"], "optional: compressing VCFs by hand"),
    ("tabix", &["--version"], "optional: indexing VCFs by hand"),
    ("docker", &["--version"], "pipeline: --caller deepvariant"),
    ("podman", &["--version"], "pipeline: --caller deepvariant (alternative runtime)"),
    ("wgsim", &[], "optional: read simulation"),
];

fn version(path: &std::path::Path, args: &[&str]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let out = Proc::new(path).args(args).output().ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    String::from_utf8_lossy(&text).lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

pub fn run(ctx: &Ctx) -> Result<()> {
    let mut rows = Vec::new();
    let required = |t: &str| matches!(t, "minimap2" | "samtools" | "bcftools");
    let mut warnings = Vec::new();
    for (tool, args, purpose) in TOOLS {
        let path = which(tool);
        let status = match (&path, required(tool)) {
            (Some(_), _) => "ok",
            (None, true) => "missing",
            (None, false) => "optional-missing",
        };
        if status == "missing" {
            warnings.push(format!("{tool} missing: {}", install_hint(tool)));
        }
        rows.push(to_record(&json!({
            "check": tool,
            "status": status,
            "detail": path.as_ref().map(|p| format!("{} {}", p.display(), version(p, args).unwrap_or_default()).trim().to_string()),
            "purpose": purpose,
            "hint": if path.is_none() { Some(install_hint(tool)) } else { None },
        })));
    }
    for c in CHAINS {
        let p = chain_path(&ctx.cache_dir, c);
        rows.push(to_record(&json!({
            "check": format!("chain {}->{}", c.from.as_str(), c.to.as_str()),
            "status": if p.exists() { "ok" } else { "not-cached" },
            "detail": p,
            "purpose": "liftover (downloaded on demand, sha256-verified)",
            "hint": if p.exists() { None } else { Some("genome liftover --fetch") },
        })));
    }
    let r = cached_reference_path(&ctx.cache_dir);
    rows.push(to_record(&json!({
        "check": "reference GRCh38",
        "status": if r.exists() { "ok" } else { "not-cached" },
        "detail": r,
        "purpose": "pipeline reference (no-alt analysis set, fetched on demand by `genome pipeline run`)",
        "hint": null,
    })));
    let mut resolver = ctx.resolver();
    for b in [Build::GRCh37, Build::GRCh38] {
        let has = resolver.has_dbsnp(b);
        rows.push(to_record(&json!({
            "check": format!("dbSNP index {}", b.as_str()),
            "status": if has { "ok" } else { "optional-missing" },
            "detail": crate::rsids::dbsnp_paths(&ctx.cache_dir, b).0,
            "purpose": "full rsid backfill for kits without rsids",
            "hint": if has { None } else { Some("genome rsid-table import DBSNP_VCF") },
        })));
    }
    let (enc_rows, enc_warnings) = crate::commands::db_cmd::status_rows(ctx);
    rows.extend(enc_rows.iter().map(to_record));
    warnings.extend(enc_warnings);
    rows.push(to_record(&json!({
        "check": "database", "status": "ok", "detail": ctx.db_path, "purpose": "kit metadata (fsqlite)", "hint": null,
    })));
    rows.push(to_record(&json!({
        "check": "data_dir", "status": "ok", "detail": ctx.data_dir, "purpose": "per-kit genotype stores",
        "hint": legacy_note(ctx),
    })));
    let access = check_private(&ctx.data_dir);
    if let Access::Open(why) = &access {
        warnings.push(format!("{} is accessible to other users ({why})", ctx.data_dir.display()));
    }
    rows.push(to_record(&json!({
        "check": "permissions",
        "status": access.status(),
        "detail": access.detail(),
        "purpose": if cfg!(windows) { "data_dir restricted to the current user (protected ACL)" } else { "data_dir owner-only (0700, files 0600)" },
        "hint": matches!(access, Access::Open(_)).then(|| permission_hint(&ctx.data_dir)),
    })));
    rows.push(key_storage_row());
    ctx.emit(&Report::new("doctor", rows).table_columns(&["check", "status", "detail", "hint"]).warnings(warnings))
}

/// Which OS credential store is in use, or why none is.
fn key_storage_row() -> Record {
    let env_key = std::env::var_os(crate::keys::KEY_ENV).is_some();
    let note = if env_key { "; GENOME_KEY is set and takes precedence for passphrase databases" } else { "" };
    let (status, detail, hint) = match keystore::backend() {
        Ok(b) => ("ok", format!("{}{note}", b.name()), None),
        Err(why) => (
            "unavailable",
            format!("{why}{note}"),
            Some("passphrase databases still work: GENOME_KEY or the interactive prompt (`genome config set kek passphrase`)"),
        ),
    };
    to_record(&json!({
        "check": "key storage", "status": status, "detail": detail,
        "purpose": "OS credential store for keyring keys and `db unlock` sessions", "hint": hint,
    }))
}

/// Set when the data dir is the pre-0.2 XDG-style macOS location.
fn legacy_note(ctx: &Ctx) -> Option<String> {
    Dir::Data.legacy().filter(|old| *old == ctx.data_dir).map(|_| {
        format!("legacy location kept because it already holds data; native: {}", Dir::Data.native().display())
    })
}

fn permission_hint(dir: &std::path::Path) -> String {
    if cfg!(windows) {
        "any command that writes (e.g. `genome import`) re-applies the owner-only ACL".into()
    } else {
        format!("chmod 700 {}", dir.display())
    }
}
