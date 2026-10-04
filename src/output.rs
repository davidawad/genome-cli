//! Rendering reports as table, JSON (versioned `genome/v1` envelope), JSONL, CSV or TSV.

use std::io::Write;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::error::{AppError, Result};

pub const SCHEMA: &str = "genome/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
    Tsv,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        <Self as clap::ValueEnum>::from_str(s, true)
            .map_err(|_| AppError::usage(format!("unknown output format '{s}' (table, json, jsonl, csv, tsv)")))
    }
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl)
    }
}

#[derive(Debug, Clone)]
pub struct OutputOpts {
    pub format: Format,
    pub output: Option<PathBuf>,
    pub precision: usize,
    pub delimiter: u8,
    pub header: bool,
    pub null: String,
    pub color: bool,
    pub columns: Option<Vec<String>>,
}

pub type Record = Map<String, Value>;

#[derive(Debug, Clone)]
pub struct Report {
    pub kind: &'static str,
    pub rows: Vec<Record>,
    /// Columns shown in table mode (all columns when empty).
    pub table_columns: Vec<String>,
    /// Columns flattened for csv/tsv when rows contain nested values.
    pub warnings: Vec<String>,
    /// Extra top-level envelope fields.
    pub meta: Record,
    /// Machine-exact rendering (no float rounding), used by export.
    pub exact: bool,
}

impl Report {
    pub fn new(kind: &'static str, rows: Vec<Record>) -> Self {
        Self { kind, rows, table_columns: Vec::new(), warnings: Vec::new(), meta: Map::new(), exact: false }
    }
    pub fn table_columns<S: AsRef<str>>(mut self, cols: &[S]) -> Self {
        self.table_columns = cols.iter().map(|c| c.as_ref().to_string()).collect();
        self
    }
    pub fn warnings(mut self, w: Vec<String>) -> Self {
        self.warnings.extend(w);
        self.warnings.dedup();
        self
    }
    pub fn meta(mut self, k: &str, v: Value) -> Self {
        self.meta.insert(k.to_string(), v);
        self
    }
    pub fn exact(mut self) -> Self {
        self.exact = true;
        self
    }
}

/// Serialize any value into a JSON object record.
pub fn to_record<T: serde::Serialize>(v: &T) -> Record {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

pub fn envelope(r: &Report) -> Value {
    let mut env = Map::new();
    env.insert("schema".into(), json!(SCHEMA));
    env.insert("kind".into(), json!(r.kind));
    env.insert("generated_at".into(), json!(crate::util::now_iso()));
    env.insert("count".into(), json!(r.rows.len()));
    r.meta.iter().for_each(|(k, v)| {
        env.insert(k.clone(), v.clone());
    });
    env.insert("data".into(), Value::Array(r.rows.iter().cloned().map(Value::Object).collect()));
    env.insert("warnings".into(), json!(r.warnings));
    Value::Object(env)
}

/// The error envelope printed on stdout in JSON modes.
pub fn error_envelope(e: &AppError) -> Value {
    json!({"schema": SCHEMA, "ok": false, "error": {"code": e.kind.as_str(), "exit_code": e.exit_code(), "message": e.message}})
}

/// Render a cell for text formats.
fn cell(v: &Value, r: &Report, o: &OutputOpts) -> String {
    match v {
        Value::Null => o.null.clone(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_f64() && !r.exact => format!("{:.*}", o.precision, n.as_f64().unwrap_or_default()),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a.iter().map(|x| cell(x, r, o)).collect::<Vec<_>>().join(","),
        Value::Object(m) => m.iter().map(|(k, x)| format!("{k}={}", cell(x, r, o))).collect::<Vec<_>>().join(";"),
    }
}

fn columns_of(rows: &[Record]) -> Vec<String> {
    rows.iter().flat_map(|r| r.keys().cloned()).fold(Vec::new(), |mut acc, k| {
        if !acc.contains(&k) {
            acc.push(k);
        }
        acc
    })
}

fn selected_columns(r: &Report, o: &OutputOpts, table: bool) -> Vec<String> {
    match &o.columns {
        Some(c) => c.clone(),
        None if table && !r.table_columns.is_empty() => {
            let all = columns_of(&r.rows);
            r.table_columns.iter().filter(|c| r.rows.is_empty() || all.contains(c)).cloned().collect()
        }
        None => columns_of(&r.rows),
    }
}

fn text_rows(r: &Report, o: &OutputOpts, cols: &[String]) -> Vec<Vec<String>> {
    r.rows
        .iter()
        .map(|row| cols.iter().map(|c| row.get(c).map_or_else(|| o.null.clone(), |v| cell(v, r, o))).collect())
        .collect()
}

fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.parse::<f64>().is_ok()
}

fn colorize(col: &str, s: &str) -> String {
    let code = match (col, s) {
        ("zygosity", "het") => "33",
        ("zygosity", "hom_alt") => "31",
        ("zygosity", "hom_ref") => "32",
        ("call_source", "inferred_ref") => "36",
        ("call_source", "missing") | ("zygosity", "no_call") => "2",
        ("status", "failed") => "31",
        ("status", "done") | ("present", "true") => "32",
        ("present", "false") => "31",
        _ => return s.to_string(),
    };
    format!("\x1b[{code}m{s}\x1b[0m")
}

pub fn render_table(r: &Report, o: &OutputOpts) -> String {
    let cols = selected_columns(r, o, true);
    let rows = text_rows(r, o, &cols);
    let mut out = String::new();
    if !rows.is_empty() {
        let width = |s: &str| s.chars().count();
        let widths: Vec<usize> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| rows.iter().map(|row| width(&row[i])).chain([width(c)]).max().unwrap_or(0))
            .collect();
        let numeric: Vec<bool> = (0..cols.len())
            .map(|i| {
                rows.iter().any(|row| is_numeric(&row[i]))
                    && rows.iter().all(|row| is_numeric(&row[i]) || row[i] == o.null)
            })
            .collect();
        let fmt_row = |row: &[String], header: bool| {
            row.iter()
                .enumerate()
                .map(|(i, s)| {
                    let pad = " ".repeat(widths[i].saturating_sub(width(s)));
                    let shown = match (o.color, header) {
                        (true, true) => format!("\x1b[1m{s}\x1b[0m"),
                        (true, false) => colorize(&cols[i], s),
                        _ => s.clone(),
                    };
                    if numeric[i] && !header {
                        format!("{pad}{shown}")
                    } else {
                        format!("{shown}{pad}")
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        let header = cols.iter().map(|c| c.to_uppercase()).collect::<Vec<_>>();
        let rule = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  ");
        let lines: Vec<String> =
            [fmt_row(&header, true), rule].into_iter().chain(rows.iter().map(|row| fmt_row(row, false))).collect();
        out = lines.join("\n") + "\n";
    }
    out
}

pub fn render_delimited(r: &Report, o: &OutputOpts, delimiter: u8) -> Result<String> {
    let cols = selected_columns(r, o, false);
    let mut w = csv::WriterBuilder::new().delimiter(delimiter).from_writer(Vec::new());
    if o.header {
        w.write_record(&cols)?;
    }
    text_rows(r, o, &cols).iter().try_for_each(|row| w.write_record(row))?;
    let bytes = w.into_inner().map_err(|e| AppError::io(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| AppError::io(e.to_string()))
}

pub fn render_jsonl(r: &Report) -> String {
    r.rows.iter().map(|v| Value::Object(v.clone()).to_string() + "\n").collect()
}

pub fn render(r: &Report, o: &OutputOpts) -> Result<String> {
    match o.format {
        Format::Table => Ok(render_table(r, o)),
        Format::Json => Ok(serde_json::to_string_pretty(&envelope(r))? + "\n"),
        Format::Jsonl => Ok(render_jsonl(r)),
        Format::Csv => render_delimited(r, o, o.delimiter),
        Format::Tsv => render_delimited(r, o, b'\t'),
    }
}

/// Write text to `--output` or stdout (ignoring a closed pipe).
pub fn write_out(text: &str, output: Option<&PathBuf>) -> Result<()> {
    match output {
        Some(p) if p.as_os_str() != "-" => {
            std::fs::write(p, text).map_err(|e| AppError::io(format!("writing {}: {e}", p.display())))
        }
        _ => {
            let mut out = std::io::stdout().lock();
            match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                other => other.map_err(AppError::from),
            }
        }
    }
}

/// Render and write to `--output` or stdout. Warnings go to stderr in
/// non-JSON modes (JSON carries them in the envelope).
pub fn emit(r: &Report, o: &OutputOpts, quiet: bool) -> Result<()> {
    if o.format != Format::Json && !quiet {
        r.warnings.iter().for_each(|w| eprintln!("genome: warning: {w}"));
    }
    write_out(&render(r, o)?, o.output.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(format: Format) -> OutputOpts {
        OutputOpts {
            format,
            output: None,
            precision: 2,
            delimiter: b',',
            header: true,
            null: "-".into(),
            color: false,
            columns: None,
        }
    }

    fn report() -> Report {
        let rows = vec![
            to_record(&json!({"rsid": "rs1", "pos": 10, "quality": 33.333, "alt": ["C", "G"], "filter": null})),
            to_record(&json!({"rsid": "rs2", "pos": 20, "quality": 1.0, "alt": [], "filter": "PASS"})),
        ];
        Report::new("genotypes", rows).warnings(vec!["w".into()])
    }

    #[test]
    fn json_envelope_is_versioned() {
        let v: Value = serde_json::from_str(&render(&report(), &opts(Format::Json)).unwrap()).unwrap();
        assert_eq!(v["schema"], "genome/v1");
        assert_eq!(v["kind"], "genotypes");
        assert_eq!(v["count"], 2);
        assert_eq!(v["data"][0]["quality"], 33.333);
        assert_eq!(v["warnings"][0], "w");
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["schema", "kind", "generated_at", "count", "data", "warnings"]);
    }

    #[test]
    fn csv_and_table() {
        let s = render(&report(), &opts(Format::Csv)).unwrap();
        assert_eq!(s, "rsid,pos,quality,alt,filter\nrs1,10,33.33,\"C,G\",-\nrs2,20,1.00,,PASS\n");
        let t = render(&report(), &opts(Format::Table)).unwrap();
        assert!(t.starts_with("RSID"));
        assert_eq!(t.lines().count(), 4);
        assert_eq!(render(&report(), &opts(Format::Jsonl)).unwrap().lines().count(), 2);
    }

    #[test]
    fn error_shape() {
        let v = error_envelope(&AppError::not_found("no kit"));
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "not_found");
    }
}
