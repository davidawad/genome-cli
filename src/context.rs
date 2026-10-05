//! Per-invocation context: resolved configuration, output options, paths.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cli::GlobalOpts;
use crate::config::{self, Layer, Resolved};
use crate::crypto::Key;
use crate::db::{Db, DbFile};
use crate::error::{AppError, ErrorKind, Result};
use crate::keys::Envelope;
use crate::output::{Format, OutputOpts, Report};

/// How this invocation protects personal data at rest.
#[derive(Clone)]
pub enum Security {
    /// `--insecure-plaintext`: nothing is encrypted.
    Plain,
    /// Encrypted database; `dek` seals genotype stores and the audit log too.
    Sealed { envelope: Box<Envelope>, dek: Key },
}

impl Security {
    pub fn key(&self) -> Option<&Key> {
        match self {
            Self::Plain => None,
            Self::Sealed { dek, .. } => Some(dek),
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub resolved: Resolved,
    pub out: OutputOpts,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
    /// `--encrypt-output`: seal `--output` files with a passphrase.
    pub encrypt_output: bool,
    pub(crate) security: Arc<Mutex<Option<Security>>>,
}

/// Report kinds that carry personal (health/genomic) data.
pub fn is_personal(kind: &str) -> bool {
    matches!(kind, "kits" | "genotypes" | "summary" | "compare" | "audit")
}

pub const PLAINTEXT_WARNING: &str =
    "genome: warning: --insecure-plaintext: personal genomic data is stored UNENCRYPTED on disk";

/// Translate global flags into the highest-precedence config layer.
pub fn flag_layer(g: &GlobalOpts) -> Layer {
    let s = |k: &str, v: &Option<String>| v.as_ref().map(|v| (k.to_string(), v.clone()));
    let p = |k: &str, v: &Option<PathBuf>| v.as_ref().map(|v| (k.to_string(), v.to_string_lossy().into_owned()));
    [
        p("data_dir", &g.data_dir),
        p("db_path", &g.db),
        p("cache_dir", &g.cache_dir),
        g.format.last().map(|f| ("format".to_string(), f.clone())),
        s("precision", &g.precision),
        s("color", &g.color),
        s("csv_delimiter", &g.delimiter),
        g.no_header.then(|| ("csv_header".to_string(), "false".to_string())),
        s("null", &g.null),
        g.offline.then(|| ("offline".to_string(), "true".to_string())),
        g.insecure_plaintext.then(|| ("insecure_plaintext".to_string(), "true".to_string())),
        g.quiet.then(|| ("quiet".to_string(), "true".to_string())),
        g.verbose.then(|| ("verbose".to_string(), "true".to_string())),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn expand_tilde(p: &str) -> String {
    let rest = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\").filter(|_| cfg!(windows)));
    match rest {
        Some(rest) => crate::platform::dirs::home().join(rest).to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

impl Ctx {
    pub fn new(g: &GlobalOpts) -> Result<Self> {
        let resolved = config::resolve(g.config.as_deref(), flag_layer(g))?;
        let to_file = g.output.as_ref().is_some_and(|p| p.as_os_str() != "-");
        let color = match resolved.get("color") {
            "always" => true,
            "never" => false,
            _ => !to_file && std::io::stdout().is_terminal(),
        };
        let out = OutputOpts {
            format: Format::parse(resolved.get("format"))?,
            output: g.output.clone(),
            precision: resolved.get("precision").parse().unwrap_or(3),
            delimiter: resolved.get("csv_delimiter").bytes().next().unwrap_or(b','),
            header: resolved.flag("csv_header"),
            null: resolved.get("null").to_string(),
            color,
            columns: g.columns.clone(),
        };
        let data_dir = PathBuf::from(expand_tilde(resolved.get("data_dir")));
        let db_path = match resolved.get("db_path") {
            "" => data_dir.join("genome.db"),
            p => PathBuf::from(expand_tilde(p)),
        };
        let cache_dir = PathBuf::from(expand_tilde(resolved.get("cache_dir")));
        Ok(Self {
            resolved,
            out,
            data_dir,
            db_path,
            cache_dir,
            encrypt_output: g.encrypt_output,
            security: Arc::new(Mutex::new(None)),
        })
    }

    pub fn insecure_plaintext(&self) -> bool {
        self.resolved.flag("insecure_plaintext")
    }

    fn cached_security(&self) -> Option<Security> {
        self.security.lock().expect("not poisoned").clone()
    }

    fn set_security(&self, s: Security) {
        *self.security.lock().expect("not poisoned") = Some(s);
    }

    /// Never create a new, empty database in front of an existing pre-0.2
    /// macOS store (it would hide the user's data).
    fn refuse_to_shadow_legacy_store(&self) -> Result<()> {
        match crate::platform::dirs::shadowed_legacy_store(&self.data_dir) {
            Some(old) => Err(AppError::new(
                ErrorKind::Config,
                format!(
                    "your genome-cli data is in {} but the data directory is {}; refusing to create a new, empty \
                     database there. Use the existing data with GENOME_DATA_DIR={} (or `genome config set data_dir`)",
                    old.display(),
                    self.data_dir.display(),
                    old.display()
                ),
            )),
            None => Ok(()),
        }
    }

    /// Open the kit database: an encrypted (sealed, in-memory) database by
    /// default, created on first use; a plaintext one only with `--insecure-plaintext`.
    pub fn db(&self) -> Result<Db> {
        self.verbose(&format!("database: {}", self.db_path.display()));
        match crate::db::inspect(&self.db_path) {
            DbFile::Missing if self.insecure_plaintext() => self.open_plain(),
            DbFile::Missing => {
                self.refuse_to_shadow_legacy_store()?;
                let (envelope, dek) = Envelope::create(self.get("kek"), &crate::prompt::Tty)?;
                let db = Db::open_sealed(&self.db_path, envelope.clone(), dek.clone())?;
                crate::setup::announce(self, &envelope);
                self.set_security(Security::Sealed { envelope: Box::new(envelope), dek });
                Ok(db)
            }
            DbFile::Plaintext if self.insecure_plaintext() => self.open_plain(),
            DbFile::Plaintext => Err(AppError::new(
                ErrorKind::Crypto,
                format!(
                    "{} is an unencrypted database: run `genome db encrypt` to encrypt it in place \
                     (or pass --insecure-plaintext to keep using it unencrypted)",
                    self.db_path.display()
                ),
            )),
            DbFile::Sealed => {
                let (envelope, dek) = match self.cached_security() {
                    Some(Security::Sealed { envelope, dek }) => (*envelope, dek),
                    _ => {
                        let envelope = crate::db::read_envelope(&self.db_path)?;
                        let dek = envelope.unlock()?;
                        (envelope, dek)
                    }
                };
                let db = Db::open_sealed(&self.db_path, envelope.clone(), dek.clone())?;
                self.set_security(Security::Sealed { envelope: Box::new(envelope), dek });
                Ok(db)
            }
            DbFile::Unknown => Err(AppError::invalid(format!(
                "{} is neither a genome-cli encrypted database nor an SQLite database",
                self.db_path.display()
            ))),
        }
    }

    fn open_plain(&self) -> Result<Db> {
        if self.cached_security().is_none() {
            eprintln!("{PLAINTEXT_WARNING} ({})", self.data_dir.display());
        }
        self.set_security(Security::Plain);
        Db::open(&self.db_path)
    }

    /// Key for genotype stores and the audit log (None in plaintext mode).
    /// Opens (and if needed unlocks) the database the first time.
    pub fn store_key(&self) -> Result<Option<Key>> {
        if self.cached_security().is_none() {
            drop(self.db()?);
        }
        Ok(self.cached_security().and_then(|s| s.key().cloned()))
    }

    /// Append an entry to the audit log (command metadata and counts only, never values).
    pub fn audit(&self, command: &str, details: serde_json::Value) -> Result<()> {
        let key = self.store_key()?;
        crate::audit::append(&self.data_dir, key.as_ref(), command, details)
    }

    /// Write rendered output to `--output` (sealed with `--encrypt-output`) or stdout.
    /// Writing personal data to a plaintext file prints a warning.
    pub fn write_output(&self, bytes: &[u8], personal: bool) -> Result<()> {
        let target = self.out.output.as_deref().filter(|p| p.as_os_str() != "-");
        if self.encrypt_output {
            let sealed = crate::keys::seal_export(bytes)?;
            return match target {
                Some(p) => crate::db::write_atomic(p, &sealed),
                None => write_stdout(&sealed),
            };
        }
        match target {
            Some(p) => {
                if personal && p != Path::new("/dev/null") {
                    eprintln!(
                        "genome: warning: writing plaintext health data to {} (use --encrypt-output to encrypt it)",
                        p.display()
                    );
                }
                std::fs::write(p, bytes).map_err(|e| AppError::io(format!("writing {}: {e}", p.display())))
            }
            None => write_stdout(bytes),
        }
    }

    /// Render a report with explicit output options and write it (see [`Ctx::write_output`]).
    pub fn emit_with(&self, r: &Report, o: &OutputOpts) -> Result<()> {
        if o.format != Format::Json && !self.quiet() {
            r.warnings.iter().for_each(|w| eprintln!("genome: warning: {w}"));
        }
        let text = zeroize::Zeroizing::new(crate::output::render(r, o)?);
        self.write_output(text.as_bytes(), is_personal(r.kind))
    }

    pub fn kits_dir(&self) -> PathBuf {
        self.data_dir.join("kits")
    }

    pub fn get(&self, key: &str) -> &str {
        self.resolved.get(key)
    }

    pub fn offline(&self) -> bool {
        self.resolved.flag("offline")
    }

    pub fn quiet(&self) -> bool {
        self.resolved.flag("quiet")
    }

    /// Informational message on stderr (suppressed by --quiet).
    pub fn info(&self, msg: &str) {
        if !self.quiet() {
            eprintln!("{msg}");
        }
    }

    pub fn verbose(&self, msg: &str) {
        if self.resolved.flag("verbose") {
            eprintln!("genome: {msg}");
        }
    }

    pub fn emit(&self, r: &Report) -> Result<()> {
        self.emit_with(r, &self.out)
    }

    pub fn lifter(&self) -> crate::liftover::Lifter {
        let info: fn(&str) = if self.quiet() { |_| {} } else { |m| eprintln!("{m}") };
        crate::liftover::Lifter::new(self.cache_dir.clone(), self.get("ucsc_url").to_string(), self.offline(), info)
    }

    pub fn resolver(&self) -> crate::rsids::Resolver {
        crate::rsids::Resolver::new(&self.cache_dir)
    }

    /// Reference FASTA (with .fai) for a build, if configured or cached.
    pub fn reference_fasta(&self, build: crate::model::Build) -> Option<PathBuf> {
        let key = match build {
            crate::model::Build::GRCh37 => "reference_grch37",
            crate::model::Build::GRCh38 => "reference_grch38",
            crate::model::Build::Unknown => return None,
        };
        let configured = Some(self.get(key)).filter(|s| !s.is_empty()).map(|s| PathBuf::from(expand_tilde(s)));
        let cached =
            (build == crate::model::Build::GRCh38).then(|| crate::pipeline::cached_reference_path(&self.cache_dir));
        configured.or(cached).filter(|p| PathBuf::from(format!("{}.fai", p.display())).exists())
    }
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(AppError::from),
    }
}
