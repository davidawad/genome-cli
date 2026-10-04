//! Thin synchronous facade over the async fsqlite (FrankenSQLite) connection.
//!
//! fsqlite exposes an async API driven by the `asupersync` runtime. The CLI is
//! a short-lived single-threaded process, so every call is simply
//! `block_on`-ed on a current-thread runtime owned by [`Db`].

use std::path::{Path, PathBuf};

use asupersync::runtime::{Runtime, RuntimeBuilder};
use fsqlite::{Connection, SqliteValue};

use crate::crypto::{self, Key};
use crate::error::{AppError, Result};
use crate::keys::Envelope;

pub type Value = SqliteValue;
pub type Row = Vec<Value>;

pub struct Db {
    rt: Runtime,
    conn: Option<Connection>,
    path: PathBuf,
    /// True while an explicit transaction is open; nested `transaction` calls join it.
    in_tx: std::cell::Cell<bool>,
    /// Encrypted mode: the database lives in memory and is re-sealed to `path`.
    sealed: Option<Sealed>,
    /// Suppress re-sealing while loading and migrating.
    loading: std::cell::Cell<bool>,
}

struct Sealed {
    dek: Key,
    envelope: Envelope,
    _lock: std::fs::File,
}

/// Magic of the sealed database container (see `docs/security.md`):
/// `GNMDBSE1 | u32 envelope_len | envelope JSON | seal(DEK, prefix, dump JSON)`.
pub const DB_MAGIC: &[u8; 8] = b"GNMDBSE1";
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// What is on disk at a database path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbFile {
    Missing,
    Plaintext,
    Sealed,
    Unknown,
}

pub fn inspect(path: &Path) -> DbFile {
    let mut head = [0u8; 16];
    match std::fs::File::open(path) {
        Err(_) => DbFile::Missing,
        Ok(mut f) => match std::io::Read::read(&mut f, &mut head) {
            Ok(0) => DbFile::Plaintext, // empty file: fsqlite initializes it as a plaintext db
            Ok(n) if n >= 8 && &head[..8] == DB_MAGIC => DbFile::Sealed,
            Ok(16) if &head == SQLITE_MAGIC => DbFile::Plaintext,
            _ => DbFile::Unknown,
        },
    }
}

/// Read a sealed container's envelope without decrypting it.
pub fn read_envelope(path: &Path) -> Result<Envelope> {
    let bytes = std::fs::read(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
    Ok(split_container(&bytes, path)?.1)
}

fn split_container<'a>(bytes: &'a [u8], path: &Path) -> Result<(&'a [u8], Envelope, &'a [u8])> {
    let bad = || AppError::invalid(format!("{} is not a genome-cli encrypted database", path.display()));
    if bytes.len() < 12 || &bytes[..8] != DB_MAGIC {
        return Err(bad());
    }
    let n = u32::from_le_bytes(bytes[8..12].try_into().expect("4")) as usize;
    let prefix = bytes.get(..12 + n).ok_or_else(bad)?;
    let env: Envelope = serde_json::from_slice(&prefix[12..]).map_err(|_| bad())?;
    Ok((prefix, env, &bytes[12 + n..]))
}

fn runtime() -> Result<Runtime> {
    RuntimeBuilder::current_thread().build().map_err(|e| AppError::db(format!("starting fsqlite runtime: {e}")))
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| AppError::io(format!("creating {}: {e}", parent.display())))?;
    }
    Ok(())
}

/// Exclusive advisory lock serializing writers of one sealed database.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".lock");
    let lp = path.with_file_name(name);
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lp)
        .map_err(|e| AppError::io(format!("{}: {e}", lp.display())))?;
    f.lock().map_err(|e| AppError::io(format!("locking {}: {e}", lp.display())))?;
    Ok(f)
}

impl Db {
    /// Open (creating if needed) the plaintext database at `path` and apply pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Self::open_raw(path)?;
        crate::store::migrate(&db)?;
        Ok(db)
    }

    /// Open a plaintext database without running migrations.
    pub fn open_raw(path: &Path) -> Result<Self> {
        create_parent(path)?;
        let rt = runtime()?;
        let p = path.to_string_lossy().into_owned();
        let conn =
            rt.block_on(Connection::open(p)).map_err(|e| AppError::db(format!("opening {}: {e}", path.display())))?;
        Self::finish_open(rt, conn, path, None)
    }

    fn finish_open(rt: Runtime, conn: Connection, path: &Path, sealed: Option<Sealed>) -> Result<Self> {
        let db = Self {
            rt,
            conn: Some(conn),
            path: path.to_path_buf(),
            in_tx: std::cell::Cell::new(false),
            sealed,
            loading: std::cell::Cell::new(true),
        };
        db.execute("PRAGMA foreign_keys = ON", &[])?;
        db.loading.set(false);
        Ok(db)
    }

    /// Open (or create, when `path` does not exist) an encrypted database. The
    /// plaintext only ever exists in an in-memory fsqlite database.
    pub fn open_sealed(path: &Path, envelope: Envelope, dek: Key) -> Result<Self> {
        create_parent(path)?;
        let lock = lock_file(path)?;
        let existing = path.exists();
        let dump = if existing {
            let bytes = std::fs::read(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
            let (prefix, _, body) = split_container(&bytes, path)?;
            Some(crypto::open(&dek, prefix, body, &path.display().to_string())?)
        } else {
            None
        };
        let rt = runtime()?;
        let conn = rt.block_on(Connection::open(":memory:".to_string())).map_err(AppError::from)?;
        let db = Self::finish_open(rt, conn, path, Some(Sealed { dek, envelope, _lock: lock }))?;
        db.loading.set(true);
        let before = match &dump {
            Some(d) => {
                let v: serde_json::Value = serde_json::from_slice(d)?;
                db.load_dump(&v)?;
                db.migration_count()?
            }
            None => -1,
        };
        crate::store::migrate(&db)?;
        db.loading.set(false);
        if db.migration_count()? != before {
            db.persist()?;
        }
        Ok(db)
    }

    fn migration_count(&self) -> Result<i64> {
        self.query_scalar_i64("SELECT COUNT(*) FROM schema_migrations", &[]).or(Ok(0))
    }

    /// Whether this database is encrypted at rest.
    pub fn is_sealed(&self) -> bool {
        self.sealed.is_some()
    }

    pub fn envelope(&self) -> Option<&Envelope> {
        self.sealed.as_ref().map(|s| &s.envelope)
    }

    /// The data-encryption key (encrypted databases only).
    pub fn dek(&self) -> Option<&Key> {
        self.sealed.as_ref().map(|s| &s.dek)
    }

    /// Replace the envelope (after `rekey`) and re-seal.
    pub fn set_envelope(&mut self, envelope: Envelope) -> Result<()> {
        if let Some(s) = self.sealed.as_mut() {
            s.envelope = envelope;
        }
        self.persist()
    }

    /// Logical dump of every table (typed values), the sealed container's payload.
    pub fn dump(&self) -> Result<serde_json::Value> {
        let mut tables = serde_json::Map::new();
        for (t, cols) in crate::store::TABLES {
            let rows = self.query(&format!("SELECT {cols} FROM {t}"), &[])?;
            let rows: Vec<serde_json::Value> =
                rows.iter().map(|r| serde_json::Value::Array(r.iter().map(value_to_json).collect())).collect();
            tables.insert((*t).to_string(), serde_json::json!({"columns": cols, "rows": rows}));
        }
        Ok(serde_json::json!({"format": "genome-cli dump v1", "tables": tables}))
    }

    /// Insert a [`Db::dump`] into this (freshly migrated or empty) database.
    pub fn load_dump(&self, dump: &serde_json::Value) -> Result<()> {
        let was = self.loading.replace(true);
        let res = (|| {
            crate::store::migrate(self)?;
            let tables = dump.get("tables").and_then(|t| t.as_object()).ok_or_else(|| AppError::invalid("bad dump"))?;
            for (t, data) in tables {
                let cols = data["columns"].as_str().unwrap_or_default();
                if !crate::store::TABLES.iter().any(|(name, c)| name == t && *c == cols) {
                    return Err(AppError::db(format!("database has unknown table layout '{t}' (newer genome-cli?)")));
                }
                self.execute(&format!("DELETE FROM {t}"), &[])?;
                let n = cols.split(',').count();
                let ph: Vec<String> = (1..=n).map(|i| format!("?{i}")).collect();
                let sql = format!("INSERT INTO {t} ({cols}) VALUES ({})", ph.join(", "));
                for row in data["rows"].as_array().into_iter().flatten() {
                    let vals: Vec<Value> = row.as_array().into_iter().flatten().map(json_to_value).collect();
                    self.execute(&sql, &vals)?;
                }
            }
            Ok(())
        })();
        self.loading.set(was);
        res
    }

    /// Re-seal the in-memory database to disk atomically (temp file, fsync, rename).
    pub fn persist(&self) -> Result<()> {
        let Some(s) = &self.sealed else { return Ok(()) };
        let dump = zeroize::Zeroizing::new(serde_json::to_vec(&self.dump()?)?);
        let env = serde_json::to_vec(&s.envelope)?;
        let mut out = Vec::with_capacity(dump.len() + env.len() + 64);
        out.extend_from_slice(DB_MAGIC);
        out.extend_from_slice(&(env.len() as u32).to_le_bytes());
        out.extend_from_slice(&env);
        let body = crypto::seal(&s.dek, &out, &dump)?;
        out.extend_from_slice(&body);
        write_atomic(&self.path, &out)
    }

    /// After a write outside an explicit transaction, re-seal.
    fn written(&self) -> Result<()> {
        if self.sealed.is_some() && !self.in_tx.get() && !self.loading.get() {
            self.persist()?;
        }
        Ok(())
    }

    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("connection is open until drop")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        let conn = self.conn();
        let r = if params.is_empty() {
            self.rt.block_on(conn.execute(sql))
        } else {
            self.rt.block_on(conn.execute_with_params(sql, params))
        };
        let n = r.map_err(AppError::from)?;
        if !sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
            self.written()?;
        }
        Ok(n)
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.rt.block_on(self.conn().execute_batch(sql)).map_err(AppError::from)?;
        self.written()
    }

    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let conn = self.conn();
        let rows = if params.is_empty() {
            self.rt.block_on(conn.query(sql))
        } else {
            self.rt.block_on(conn.query_with_params(sql, params))
        }?;
        Ok(rows.into_iter().map(|r| r.values().to_vec()).collect())
    }

    pub fn query_opt(&self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        Ok(self.query(sql, params)?.into_iter().next())
    }

    pub fn query_scalar_i64(&self, sql: &str, params: &[Value]) -> Result<i64> {
        self.query_opt(sql, params)?
            .and_then(|r| r.first().and_then(as_i64))
            .ok_or_else(|| AppError::db(format!("expected integer result from: {sql}")))
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.conn().last_insert_rowid()
    }

    pub fn begin(&self) -> Result<()> {
        self.rt.block_on(self.conn().begin_transaction())?;
        self.in_tx.set(true);
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.in_tx.set(false);
        self.rt.block_on(self.conn().commit_transaction()).map_err(AppError::from)?;
        self.written()
    }

    pub fn rollback(&self) -> Result<()> {
        self.in_tx.set(false);
        self.rt.block_on(self.conn().rollback_transaction()).map_err(AppError::from)
    }

    /// Run `f` inside a transaction, committing on success and rolling back on
    /// error. When a transaction is already open, `f` simply joins it and the
    /// outer owner decides whether to commit.
    pub fn transaction<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if self.in_tx.get() {
            return f(self);
        }
        self.begin()?;
        match f(self) {
            Ok(v) => self.commit().map(|()| v),
            Err(e) => {
                let _ = self.rollback();
                Err(e)
            }
        }
    }

    /// Exact page-level copy of a plaintext database to `target`.
    pub fn backup_to(&self, target: &Path) -> Result<()> {
        self.rt
            .block_on(self.conn().backup_exact_to(target))
            .map(|_| ())
            .map_err(|e| AppError::db(format!("backup to {}: {e}", target.display())))
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = self.rt.block_on(conn.close());
        }
    }
}

/// Write `bytes` to `path` via a temp file, fsync and rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = crypto::tmp_path(path);
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppError::io(format!("writing {}: {e}", path.display())));
    }
    crypto::sync_parent(path);
    Ok(())
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!({ "real": f }),
        Value::Text(s) => serde_json::Value::String(s.to_string()),
        Value::Blob(b) => serde_json::json!({ "blob": crypto::hex(b) }),
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Number(n) => {
            n.as_i64().map_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0)), Value::Integer)
        }
        serde_json::Value::String(s) => text(s),
        serde_json::Value::Object(o) => match (o.get("real"), o.get("blob")) {
            (Some(f), _) => Value::Float(f.as_f64().unwrap_or(0.0)),
            (_, Some(b)) => Value::from(crypto::unhex(b.as_str().unwrap_or_default()).unwrap_or_default()),
            _ => Value::Null,
        },
        other => text(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

pub fn text(s: impl AsRef<str>) -> Value {
    Value::from(s.as_ref())
}

pub fn opt_text<S: AsRef<str>>(s: Option<S>) -> Value {
    s.map_or(Value::Null, text)
}

pub fn real(f: f64) -> Value {
    Value::Float(f)
}

pub fn opt_real(f: Option<f64>) -> Value {
    f.map_or(Value::Null, Value::Float)
}

pub fn int(i: i64) -> Value {
    Value::Integer(i)
}

pub fn opt_int(i: Option<i64>) -> Value {
    i.map_or(Value::Null, Value::Integer)
}

pub fn opt_bool(b: Option<bool>) -> Value {
    b.map_or(Value::Null, |b| Value::Integer(i64::from(b)))
}

pub fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Float(f) => Some(*f as i64),
        Value::Text(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Text(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn as_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(s) => Some(s.to_string()),
        other => Some(other.to_text()),
    }
}

/// Column accessors for a row.
pub trait RowExt {
    fn s(&self, i: usize) -> Option<String>;
    fn f(&self, i: usize) -> Option<f64>;
    fn i(&self, i: usize) -> Option<i64>;
    fn b(&self, i: usize) -> Option<bool> {
        self.i(i).map(|v| v != 0)
    }
}

impl RowExt for [Value] {
    fn s(&self, i: usize) -> Option<String> {
        self.get(i).and_then(as_string)
    }
    fn f(&self, i: usize) -> Option<f64> {
        self.get(i).and_then(as_f64)
    }
    fn i(&self, i: usize) -> Option<i64> {
        self.get(i).and_then(as_i64)
    }
}
