//! Downloads into the cache with checksum verification and atomic rename.

use std::io::{Read, Write};
use std::path::Path;

use md5::Md5;
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy)]
pub enum Checksum<'a> {
    Sha256(&'a str),
    Md5(&'a str),
}

/// Download `url` to `dest`, verifying `checksum` before the file appears at `dest`.
pub fn download(url: &str, dest: &Path, checksum: Option<Checksum>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError::io(format!("{}: {e}", parent.display())))?;
    }
    let tmp = dest.with_extension("part");
    let resp = ureq::get(url).call().map_err(|e| AppError::network(format!("GET {url}: {e}")))?;
    let mut reader = resp.into_reader();
    let mut out = std::fs::File::create(&tmp).map_err(|e| AppError::io(format!("{}: {e}", tmp.display())))?;
    let (mut sha, mut md5) = (Sha256::new(), Md5::new());
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf).map_err(|e| AppError::network(format!("reading {url}: {e}")))?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        md5.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.flush()?;
    drop(out);
    let (got, want) = match checksum {
        Some(Checksum::Sha256(w)) => (format!("{:x}", sha.finalize()), Some(w)),
        Some(Checksum::Md5(w)) => (format!("{:x}", md5.finalize()), Some(w)),
        None => (String::new(), None),
    };
    if let Some(w) = want {
        if !got.eq_ignore_ascii_case(w) {
            let _ = std::fs::remove_file(&tmp);
            return Err(AppError::network(format!("checksum mismatch for {url}: expected {w}, got {got}")));
        }
    }
    std::fs::rename(&tmp, dest).map_err(|e| AppError::io(format!("{}: {e}", dest.display())))
}
