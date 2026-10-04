//! Kit metadata in fsqlite. Genotypes live in per-kit stores (`gtstore`).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::db::{int, opt_text, text, Db, Row, RowExt};
use crate::error::{AppError, Result};

const SCHEMA_V1: &str = r"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version     INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    applied_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS kits (
    seq             INTEGER PRIMARY KEY,
    id              TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL UNIQUE,
    source_format   TEXT NOT NULL,
    assay           TEXT NOT NULL,
    build           TEXT NOT NULL,
    build_evidence  TEXT NOT NULL,
    sample          TEXT,
    records         INTEGER NOT NULL,
    has_rsids       INTEGER NOT NULL,
    rsid_records    INTEGER NOT NULL DEFAULT 0,
    ref_calls       TEXT NOT NULL,
    chip            TEXT,
    imported_at     TEXT NOT NULL,
    source_path     TEXT NOT NULL,
    store_dir       TEXT NOT NULL,
    summary_json    TEXT NOT NULL,
    warnings_json   TEXT NOT NULL DEFAULT '[]'
);
";

pub fn migrate(db: &Db) -> Result<()> {
    db.execute_batch(SCHEMA_V1)?;
    if db.query_opt("SELECT version FROM schema_migrations WHERE version = 1", &[])?.is_none() {
        db.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (1, 'kits', ?1)",
            &[text(crate::util::now_iso())],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Kit {
    pub id: String,
    pub name: String,
    pub source_format: String,
    pub assay: String,
    pub build: String,
    pub build_evidence: String,
    pub sample: Option<String>,
    pub records: i64,
    pub has_rsids: bool,
    pub ref_calls: String,
    pub chip: Option<String>,
    pub imported_at: String,
    pub source_path: String,
    #[serde(skip)]
    pub seq: i64,
    #[serde(skip)]
    pub rsid_records: i64,
    #[serde(skip)]
    pub store_dir: String,
    #[serde(skip)]
    pub summary: Map<String, Value>,
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Kit {
    pub fn build(&self) -> crate::model::Build {
        crate::model::Build::parse(&self.build).unwrap_or(crate::model::Build::Unknown)
    }
    pub fn absent_means_ref(&self) -> bool {
        self.ref_calls == "absent-means-ref"
    }
}

const COLS: &str = "seq, id, name, source_format, assay, build, build_evidence, sample, records, has_rsids, \
                    rsid_records, ref_calls, chip, imported_at, source_path, store_dir, summary_json, warnings_json";

fn from_row(r: &Row) -> Kit {
    let r = r.as_slice();
    let json = |i: usize| r.s(i).unwrap_or_default();
    Kit {
        seq: r.i(0).unwrap_or(0),
        id: r.s(1).unwrap_or_default(),
        name: r.s(2).unwrap_or_default(),
        source_format: r.s(3).unwrap_or_default(),
        assay: r.s(4).unwrap_or_default(),
        build: r.s(5).unwrap_or_default(),
        build_evidence: r.s(6).unwrap_or_default(),
        sample: r.s(7),
        records: r.i(8).unwrap_or(0),
        has_rsids: r.b(9).unwrap_or(false),
        rsid_records: r.i(10).unwrap_or(0),
        ref_calls: r.s(11).unwrap_or_default(),
        chip: r.s(12),
        imported_at: r.s(13).unwrap_or_default(),
        source_path: r.s(14).unwrap_or_default(),
        store_dir: r.s(15).unwrap_or_default(),
        summary: serde_json::from_str(&json(16)).unwrap_or_default(),
        warnings: serde_json::from_str(&json(17)).unwrap_or_default(),
    }
}

pub fn list(db: &Db) -> Result<Vec<Kit>> {
    Ok(db.query(&format!("SELECT {COLS} FROM kits ORDER BY seq"), &[])?.iter().map(from_row).collect())
}

/// Find a kit by id (`k3`) or name.
pub fn get(db: &Db, key: &str) -> Result<Kit> {
    db.query_opt(
        &format!("SELECT {COLS} FROM kits WHERE id = ?1 OR name = ?1 ORDER BY (id = ?1) DESC LIMIT 1"),
        &[text(key)],
    )?
    .map(|r| from_row(&r))
    .ok_or_else(|| AppError::not_found(format!("no kit '{key}' (see `genome kits`)")))
}

pub fn next_seq(db: &Db) -> Result<i64> {
    db.query_scalar_i64("SELECT COALESCE(MAX(seq), 0) + 1 FROM kits", &[])
}

pub fn name_taken(db: &Db, name: &str) -> Result<bool> {
    Ok(db.query_opt("SELECT 1 FROM kits WHERE name = ?1 OR id = ?1", &[text(name)])?.is_some())
}

pub fn insert(db: &Db, k: &Kit) -> Result<()> {
    db.execute(
        &format!("INSERT INTO kits ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"),
        &[
            int(k.seq),
            text(&k.id),
            text(&k.name),
            text(&k.source_format),
            text(&k.assay),
            text(&k.build),
            text(&k.build_evidence),
            opt_text(k.sample.as_deref()),
            int(k.records),
            int(i64::from(k.has_rsids)),
            int(k.rsid_records),
            text(&k.ref_calls),
            opt_text(k.chip.as_deref()),
            text(&k.imported_at),
            text(&k.source_path),
            text(&k.store_dir),
            text(serde_json::to_string(&k.summary)?),
            text(serde_json::to_string(&k.warnings)?),
        ],
    )?;
    Ok(())
}

pub fn delete(db: &Db, id: &str) -> Result<()> {
    db.execute("DELETE FROM kits WHERE id = ?1", &[text(id)])?;
    Ok(())
}

pub fn rename(db: &Db, id: &str, name: &str) -> Result<()> {
    db.execute("UPDATE kits SET name = ?2 WHERE id = ?1", &[text(id), text(name)])?;
    Ok(())
}
