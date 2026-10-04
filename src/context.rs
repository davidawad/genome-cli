//! Per-invocation context: resolved configuration, output options, paths.

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::GlobalOpts;
use crate::config::{self, Layer, Resolved};
use crate::db::Db;
use crate::error::Result;
use crate::output::{Format, OutputOpts, Report};

#[derive(Clone)]
pub struct Ctx {
    pub resolved: Resolved,
    pub out: OutputOpts,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
}

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
        g.quiet.then(|| ("quiet".to_string(), "true".to_string())),
        g.verbose.then(|| ("verbose".to_string(), "true".to_string())),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn expand_tilde(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
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
        Ok(Self { resolved, out, data_dir, db_path, cache_dir })
    }

    pub fn db(&self) -> Result<Db> {
        self.verbose(&format!("database: {}", self.db_path.display()));
        Db::open(&self.db_path)
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
        crate::output::emit(r, &self.out, self.quiet())
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
