//! Thin synchronous facade over the async fsqlite (FrankenSQLite) connection.
//!
//! fsqlite exposes an async API driven by the `asupersync` runtime. The CLI is
//! a short-lived single-threaded process, so every call is simply
//! `block_on`-ed on a current-thread runtime owned by [`Db`].

use std::path::{Path, PathBuf};

use asupersync::runtime::{Runtime, RuntimeBuilder};
use fsqlite::{Connection, SqliteValue};

use crate::error::{AppError, Result};

pub type Value = SqliteValue;
pub type Row = Vec<Value>;

pub struct Db {
    rt: Runtime,
    conn: Option<Connection>,
    path: PathBuf,
    /// True while an explicit transaction is open; nested `transaction` calls join it.
    in_tx: std::cell::Cell<bool>,
}

impl Db {
    /// Open (creating if needed) the database at `path` and apply pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Self::open_raw(path)?;
        crate::store::migrate(&db)?;
        Ok(db)
    }

    /// Open without running migrations.
    pub fn open_raw(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(format!("creating {}: {e}", parent.display())))?;
        }
        let rt = RuntimeBuilder::current_thread()
            .build()
            .map_err(|e| AppError::db(format!("starting fsqlite runtime: {e}")))?;
        let p = path.to_string_lossy().into_owned();
        let conn =
            rt.block_on(Connection::open(p)).map_err(|e| AppError::db(format!("opening {}: {e}", path.display())))?;
        let db = Self { rt, conn: Some(conn), path: path.to_path_buf(), in_tx: std::cell::Cell::new(false) };
        db.execute("PRAGMA foreign_keys = ON", &[])?;
        Ok(db)
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
        r.map_err(AppError::from)
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.rt.block_on(self.conn().execute_batch(sql)).map_err(AppError::from)
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
        self.rt.block_on(self.conn().commit_transaction()).map_err(AppError::from)
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

    /// Exact page-level copy of the database to `target`.
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
