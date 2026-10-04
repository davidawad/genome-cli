//! Application error type with stable, documented process exit codes.

use std::fmt;

/// Error categories. Each maps to a stable exit code (see `docs/json-schema.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Unspecified failure.
    General,
    /// Bad command-line usage or invalid argument value.
    Usage,
    /// A referenced kit, rsid, chain file, ... does not exist.
    NotFound,
    /// Input data failed validation (unparseable genotype file, bad region, ...).
    Invalid,
    /// The database engine reported an error.
    Database,
    /// Filesystem / IO failure.
    Io,
    /// Configuration file or value problem.
    Config,
    /// Download / network failure (chain files, reference genome).
    Network,
    /// An external tool (minimap2, samtools, bcftools, ...) is missing or failed.
    Tool,
    /// Missing or wrong key, or encrypted data failed authentication (tampered/corrupt).
    Crypto,
}

impl ErrorKind {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::General => 1,
            Self::Usage => 2,
            Self::NotFound => 3,
            Self::Invalid => 4,
            Self::Database => 5,
            Self::Io => 6,
            Self::Config => 7,
            Self::Network => 8,
            Self::Tool => 9,
            Self::Crypto => 10,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Usage => "usage",
            Self::NotFound => "not_found",
            Self::Invalid => "invalid",
            Self::Database => "database",
            Self::Io => "io",
            Self::Config => "config",
            Self::Network => "network",
            Self::Tool => "tool",
            Self::Crypto => "crypto",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppError {
    pub kind: ErrorKind,
    pub message: String,
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;

impl AppError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn usage(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, m)
    }
    pub fn db(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Database, m)
    }
    pub fn io(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, m)
    }
    pub fn config(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, m)
    }
    pub fn network(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, m)
    }
    pub fn tool(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Tool, m)
    }
    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }
    /// Prefix the message with additional context.
    pub fn context(self, ctx: impl fmt::Display) -> Self {
        Self { kind: self.kind, message: format!("{ctx}: {}", self.message) }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::io(e.to_string())
    }
}

impl From<fsqlite::FrankenError> for AppError {
    fn from(e: fsqlite::FrankenError) -> Self {
        let msg = e.to_string();
        if msg.contains("UNIQUE constraint") {
            Self::invalid(msg)
        } else {
            Self::db(msg)
        }
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid(format!("JSON: {e}"))
    }
}

impl From<csv::Error> for AppError {
    fn from(e: csv::Error) -> Self {
        Self::invalid(format!("CSV: {e}"))
    }
}

/// Extension to attach context to any `Result<_, AppError>`.
pub trait ResultExt<T> {
    fn ctx(self, ctx: impl fmt::Display) -> Result<T>;
}

impl<T, E: Into<AppError>> ResultExt<T> for std::result::Result<T, E> {
    fn ctx(self, ctx: impl fmt::Display) -> Result<T> {
        self.map_err(|e| e.into().context(ctx))
    }
}
