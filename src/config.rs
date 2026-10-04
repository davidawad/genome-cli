//! Layered configuration: built-in defaults < config file < `GENOME_*`
//! environment variables < command-line flags. Every resolved value remembers
//! the layer it came from (`config show --effective`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    File,
    Env,
    Flag,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

pub struct Setting {
    pub key: &'static str,
    pub env: &'static [&'static str],
    pub help: &'static str,
    pub choices: &'static [&'static str],
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Str,
    Bool,
    Uint,
    Count,
    Char,
}

pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "data_dir",
        env: &["GENOME_DATA_DIR"],
        help: "directory holding the kit database and per-kit genotype stores",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "db_path",
        env: &["GENOME_DB", "GENOME_DB_PATH"],
        help: "kit metadata database (default: <data_dir>/genome.db)",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "cache_dir",
        env: &["GENOME_CACHE_DIR"],
        help: "download cache: chain files, reference genome, dbSNP index",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "format",
        env: &["GENOME_FORMAT"],
        help: "default output format",
        choices: &["table", "json", "jsonl", "csv", "tsv"],
        kind: Kind::Str,
    },
    Setting {
        key: "color",
        env: &["GENOME_COLOR"],
        help: "colour table output",
        choices: &["auto", "always", "never"],
        kind: Kind::Str,
    },
    Setting {
        key: "precision",
        env: &["GENOME_PRECISION"],
        help: "decimal places in table/csv/tsv output",
        choices: &[],
        kind: Kind::Uint,
    },
    Setting {
        key: "csv_delimiter",
        env: &["GENOME_CSV_DELIMITER"],
        help: "CSV field delimiter (single character, or 'tab')",
        choices: &[],
        kind: Kind::Char,
    },
    Setting {
        key: "csv_header",
        env: &["GENOME_CSV_HEADER"],
        help: "write a header row in csv/tsv output",
        choices: &[],
        kind: Kind::Bool,
    },
    Setting {
        key: "null",
        env: &["GENOME_NULL"],
        help: "text for missing values in table/csv/tsv output",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "max_discordant",
        env: &["GENOME_MAX_DISCORDANT"],
        help: "cap on discordant sites listed by compare",
        choices: &[],
        kind: Kind::Count,
    },
    Setting {
        key: "threads",
        env: &["GENOME_THREADS"],
        help: "threads passed to external pipeline tools",
        choices: &[],
        kind: Kind::Count,
    },
    Setting {
        key: "aligner",
        env: &["GENOME_ALIGNER"],
        help: "pipeline aligner",
        choices: &["minimap2", "bwa-mem2"],
        kind: Kind::Str,
    },
    Setting {
        key: "caller",
        env: &["GENOME_CALLER"],
        help: "pipeline variant caller",
        choices: &["bcftools", "deepvariant"],
        kind: Kind::Str,
    },
    Setting {
        key: "container",
        env: &["GENOME_CONTAINER"],
        help: "container runtime for deepvariant",
        choices: &["auto", "docker", "podman"],
        kind: Kind::Str,
    },
    Setting {
        key: "reference",
        env: &["GENOME_REFERENCE"],
        help: "pipeline reference: GRCh38 (fetched into the cache) or a FASTA path",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "reference_grch37",
        env: &["GENOME_REFERENCE_GRCH37"],
        help: "optional local GRCh37 FASTA (+ .fai) used to fill reference alleles",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "reference_grch38",
        env: &["GENOME_REFERENCE_GRCH38"],
        help: "optional local GRCh38 FASTA (+ .fai) used to fill reference alleles",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "ucsc_url",
        env: &["GENOME_UCSC_URL"],
        help: "base URL for UCSC liftOver chain files",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "reference_url",
        env: &["GENOME_REFERENCE_URL"],
        help: "URL of the GRCh38 no-alt analysis set FASTA (.fna.gz)",
        choices: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "offline",
        env: &["GENOME_OFFLINE"],
        help: "never download; fail if a cached file is missing",
        choices: &[],
        kind: Kind::Bool,
    },
    Setting {
        key: "quiet",
        env: &["GENOME_QUIET"],
        help: "suppress informational messages",
        choices: &[],
        kind: Kind::Bool,
    },
    Setting {
        key: "verbose",
        env: &["GENOME_VERBOSE"],
        help: "print extra diagnostics to stderr",
        choices: &[],
        kind: Kind::Bool,
    },
];

pub fn setting(key: &str) -> Option<&'static Setting> {
    let k = key.replace('-', "_");
    SETTINGS.iter().find(|s| s.key == k)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

pub fn default_config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("genome-cli").join("config.toml")
}

pub fn default_data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("genome-cli")
}

pub fn default_cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("genome-cli")
}

fn default_value(key: &str) -> String {
    match key {
        "data_dir" => default_data_dir().to_string_lossy().into_owned(),
        "cache_dir" => default_cache_dir().to_string_lossy().into_owned(),
        "format" => "table".into(),
        "color" => "auto".into(),
        "precision" => "3".into(),
        "csv_delimiter" => ",".into(),
        "csv_header" => "true".into(),
        "max_discordant" => "50".into(),
        "threads" => "4".into(),
        "aligner" => "minimap2".into(),
        "caller" => "bcftools".into(),
        "container" => "auto".into(),
        "reference" => "GRCh38".into(),
        "ucsc_url" => "https://hgdownload.soe.ucsc.edu/goldenPath".into(),
        "reference_url" => crate::pipeline::GRCH38_NO_ALT_URL.into(),
        "offline" | "quiet" | "verbose" => "false".into(),
        _ => String::new(),
    }
}

/// Validate and normalise a raw value for `key`.
pub fn normalize(key: &str, raw: &str) -> Result<String> {
    let s = setting(key).ok_or_else(|| AppError::config(format!("unknown config key '{key}'")))?;
    let v = raw.trim();
    let bad = |why: &str| AppError::config(format!("invalid value '{raw}' for {}: {why}", s.key));
    match s.kind {
        Kind::Bool => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok("true".into()),
            "0" | "false" | "no" | "off" | "" => Ok("false".into()),
            _ => Err(bad("expected true/false")),
        },
        Kind::Count => v.parse::<u64>().map(|n| n.to_string()).map_err(|_| bad("expected a non-negative integer")),
        Kind::Uint => {
            v.parse::<u8>().map(|n| n.min(12).to_string()).map_err(|_| bad("expected a small non-negative integer"))
        }
        Kind::Char => match raw {
            "tab" | "\\t" | "\t" => Ok("\t".into()),
            c if c.chars().count() == 1 && c.is_ascii() => Ok(c.into()),
            _ => Err(bad("expected a single ASCII character or 'tab'")),
        },
        Kind::Str if !s.choices.is_empty() => {
            let l = v.to_ascii_lowercase();
            if s.choices.contains(&l.as_str()) {
                Ok(l)
            } else {
                Err(bad(&format!("expected one of {}", s.choices.join(", "))))
            }
        }
        Kind::Str => Ok(v.to_string()),
    }
}

/// One layer of raw key/value pairs.
pub type Layer = Vec<(String, String)>;

/// Resolved settings with provenance, in `SETTINGS` order.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub values: BTreeMap<&'static str, (String, Source)>,
    pub config_path: PathBuf,
    pub config_path_source: Source,
    pub config_file_exists: bool,
}

impl Resolved {
    pub fn get(&self, key: &str) -> &str {
        self.values.get(key).map_or("", |(v, _)| v.as_str())
    }
    pub fn source(&self, key: &str) -> Source {
        self.values.get(key).map_or(Source::Default, |(_, s)| *s)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.get(key) == "true"
    }
    /// Settings in canonical display order.
    pub fn ordered(&self) -> Vec<(&'static str, &str, Source)> {
        SETTINGS.iter().filter_map(|s| self.values.get(s.key).map(|(v, src)| (s.key, v.as_str(), *src))).collect()
    }
}

/// Fold layers (lowest precedence first) over the defaults.
pub fn resolve_layers(layers: &[(Source, Layer)]) -> Result<BTreeMap<&'static str, (String, Source)>> {
    let defaults =
        SETTINGS.iter().map(|s| (s.key, (default_value(s.key), Source::Default))).collect::<BTreeMap<_, _>>();
    layers.iter().try_fold(defaults, |mut acc, (src, layer)| {
        layer.iter().try_for_each(|(k, v)| {
            let s =
                setting(k).ok_or_else(|| AppError::config(format!("unknown config key '{k}' ({})", src.as_str())))?;
            let v = normalize(s.key, v).map_err(|e| e.context(src.as_str()))?;
            acc.insert(s.key, (v, *src));
            Ok::<_, AppError>(())
        })?;
        Ok(acc)
    })
}

/// Locate the config file: `--config` > `GENOME_CONFIG` > XDG default.
pub fn locate(flag: Option<&Path>) -> (PathBuf, Source) {
    flag.map(|p| (p.to_path_buf(), Source::Flag))
        .or_else(|| {
            std::env::var_os("GENOME_CONFIG").filter(|v| !v.is_empty()).map(|v| (PathBuf::from(v), Source::Env))
        })
        .unwrap_or_else(|| (default_config_path(), Source::Default))
}

/// Read the config file as a flat layer (missing default file = empty layer).
pub fn file_layer(path: &Path, explicit: bool) -> Result<Layer> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_toml_layer(&text).map_err(|e| e.context(path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Vec::new()),
        Err(e) => Err(AppError::config(format!("reading config {}: {e}", path.display()))),
    }
}

fn toml_scalar(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Accepts flat keys and one level of tables (`[csv] delimiter = ";"` ==
/// `csv_delimiter = ";"`).
pub fn parse_toml_layer(text: &str) -> Result<Layer> {
    let table: toml::Table = text.parse().map_err(|e| AppError::config(format!("TOML: {e}")))?;
    table
        .iter()
        .flat_map(|(k, v)| match v {
            toml::Value::Table(t) => t.iter().map(|(k2, v2)| (format!("{k}_{k2}"), v2.clone())).collect::<Vec<_>>(),
            other => vec![(k.clone(), other.clone())],
        })
        .map(|(k, v)| {
            toml_scalar(&v)
                .map(|s| (k.clone(), s))
                .ok_or_else(|| AppError::config(format!("config key '{k}' must be a string, number or boolean")))
        })
        .collect()
}

/// `GENOME_*` environment layer.
pub fn env_layer() -> Layer {
    SETTINGS
        .iter()
        .filter_map(|s| {
            s.env.iter().find_map(|e| std::env::var(e).ok().filter(|v| !v.is_empty())).map(|v| (s.key.to_string(), v))
        })
        .chain(
            std::env::var_os("NO_COLOR")
                .filter(|v| !v.is_empty())
                .filter(|_| std::env::var_os("GENOME_COLOR").is_none())
                .map(|_| ("color".to_string(), "never".to_string())),
        )
        .collect()
}

pub fn resolve(config_flag: Option<&Path>, flags: Layer) -> Result<Resolved> {
    let (config_path, config_path_source) = locate(config_flag);
    let file = file_layer(&config_path, config_path_source != Source::Default)?;
    let values = resolve_layers(&[(Source::File, file), (Source::Env, env_layer()), (Source::Flag, flags)])?;
    Ok(Resolved { values, config_file_exists: config_path.exists(), config_path, config_path_source })
}

/// Set `key = value` in the TOML file at `path`, creating it if needed.
pub fn write_setting(path: &Path, key: &str, value: Option<&str>) -> Result<()> {
    let s = setting(key).ok_or_else(|| AppError::config(format!("unknown config key '{key}'")))?;
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut table: toml::Table = text.parse().map_err(|e| AppError::config(format!("{}: {e}", path.display())))?;
    match value {
        Some(v) => {
            let v = normalize(s.key, v)?;
            let tv = match s.kind {
                Kind::Bool => toml::Value::Boolean(v == "true"),
                Kind::Uint | Kind::Count => toml::Value::Integer(v.parse().unwrap_or(0)),
                _ => toml::Value::String(v),
            };
            table.insert(s.key.to_string(), tv);
        }
        None => {
            table.remove(s.key);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = toml::to_string_pretty(&table).map_err(|e| AppError::config(e.to_string()))?;
    std::fs::write(path, out).map_err(|e| AppError::io(format!("writing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(pairs: &[(&str, &str)]) -> Layer {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn layers_override_in_order() {
        let r = resolve_layers(&[
            (Source::File, layer(&[("format", "csv"), ("precision", "4")])),
            (Source::Env, layer(&[("format", "json")])),
            (Source::Flag, layer(&[("precision", "1")])),
        ])
        .unwrap();
        assert_eq!(r["format"], ("json".to_string(), Source::Env));
        assert_eq!(r["precision"], ("1".to_string(), Source::Flag));
        assert_eq!(r["caller"], ("bcftools".to_string(), Source::Default));
        assert_eq!(r["color"], ("auto".to_string(), Source::Default));
    }

    #[test]
    fn validates_values() {
        assert!(resolve_layers(&[(Source::File, layer(&[("format", "xml")]))]).is_err());
        assert!(resolve_layers(&[(Source::File, layer(&[("nope", "1")]))]).is_err());
        assert_eq!(normalize("csv_delimiter", "tab").unwrap(), "\t");
        assert_eq!(normalize("quiet", "YES").unwrap(), "true");
        assert_eq!(normalize("threads", "16").unwrap(), "16");
        assert!(normalize("caller", "gatk").is_err());
    }

    #[test]
    fn toml_tables_flatten() {
        let l =
            parse_toml_layer("format = \"json\"\nprecision = 3\n[csv]\ndelimiter = \";\"\nheader = false\n").unwrap();
        assert!(l.contains(&("csv_delimiter".into(), ";".into())));
        assert!(l.contains(&("csv_header".into(), "false".into())));
        assert!(l.contains(&("precision".into(), "3".into())));
    }
}
