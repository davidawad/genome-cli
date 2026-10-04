//! Append-only, encrypted, hash-chained audit log of commands that read or
//! modify personal data: timestamp, command, kit ids and counts, never values.
//!
//! `<data_dir>/audit.log`:
//!
//! ```text
//! header:  "GNMAUDT1" | file_id [16]
//! record:  seq u64 | len u32 | seal(DEK, aad, entry JSON) (len bytes) | len u32
//! aad:     "genome-cli audit v1" | file_id | seq | tag of the previous record (zeros for the first)
//! ```
//!
//! Chaining each record to the previous record's tag makes removing,
//! reordering or editing any record (other than truncating the tail) fail
//! verification. With `--insecure-plaintext` the log is `audit.jsonl`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::crypto::{self, Key, TAG_LEN};
use crate::error::{AppError, ErrorKind, Result};

const MAGIC: &[u8; 8] = b"GNMAUDT1";
const HEADER: u64 = 24;

pub fn sealed_path(data_dir: &Path) -> PathBuf {
    data_dir.join("audit.log")
}

pub fn plain_path(data_dir: &Path) -> PathBuf {
    data_dir.join("audit.jsonl")
}

fn aad(file_id: &[u8], seq: u64, prev: &[u8]) -> Vec<u8> {
    [b"genome-cli audit v1|".as_slice(), file_id, &seq.to_le_bytes(), prev].concat()
}

fn tamper(p: &Path, why: impl std::fmt::Display) -> AppError {
    AppError::new(ErrorKind::Crypto, format!("audit log {} failed verification: {why}", p.display()))
}

fn entry(seq: u64, command: &str, details: Value) -> Value {
    json!({
        "seq": seq,
        "ts": crate::util::now_iso(),
        "command": command,
        "user": std::env::var("USER").ok(),
        "details": details,
    })
}

/// Append one entry (sealed with `key`, or plaintext JSONL without one).
pub fn append(data_dir: &Path, key: Option<&Key>, command: &str, details: Value) -> Result<()> {
    crate::util::private_dir(data_dir)?;
    match key {
        Some(k) => append_sealed(&sealed_path(data_dir), k, command, details),
        None => {
            let p = plain_path(data_dir);
            let mut f = OpenOptions::new().create(true).append(true).open(&p)?;
            f.lock()?;
            let seq = std::fs::read_to_string(&p).map(|s| s.lines().count() as u64).unwrap_or(0) + 1;
            writeln!(f, "{}", entry(seq, command, details))?;
            f.sync_all()?;
            Ok(())
        }
    }
}

fn append_sealed(p: &Path, key: &Key, command: &str, details: Value) -> Result<()> {
    let mut f = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(p)?;
    f.lock()?;
    let size = f.metadata()?.len();
    let mut header = [0u8; HEADER as usize];
    if size == 0 {
        header[..8].copy_from_slice(MAGIC);
        crypto::fill_random(&mut header[8..])?;
        f.write_all(&header)?;
    } else {
        f.read_exact(&mut header).map_err(|_| tamper(p, "bad header"))?;
        if &header[..8] != MAGIC {
            return Err(tamper(p, "bad header"));
        }
    }
    let (last_seq, prev) =
        if size > HEADER { last_record(&mut f, size).map_err(|e| tamper(p, e))? } else { (0, [0u8; TAG_LEN]) };
    let seq = last_seq + 1;
    let plain = serde_json::to_vec(&entry(seq, command, details))?;
    let sealed = crypto::seal(key, &aad(&header[8..], seq, &prev), &plain)?;
    let len = (sealed.len() as u32).to_le_bytes();
    let mut rec = Vec::with_capacity(sealed.len() + 16);
    rec.extend_from_slice(&seq.to_le_bytes());
    rec.extend_from_slice(&len);
    rec.extend_from_slice(&sealed);
    rec.extend_from_slice(&len);
    f.seek(SeekFrom::End(0))?;
    f.write_all(&rec)?;
    f.sync_all()?;
    Ok(())
}

/// (seq, tag) of the last record, found through its trailing length.
fn last_record(f: &mut File, size: u64) -> std::io::Result<(u64, [u8; TAG_LEN])> {
    let mut b4 = [0u8; 4];
    f.seek(SeekFrom::Start(size - 4))?;
    f.read_exact(&mut b4)?;
    let len = u64::from(u32::from_le_bytes(b4));
    let start = size.checked_sub(4 + len + 12).filter(|s| *s >= HEADER).ok_or(std::io::ErrorKind::InvalidData)?;
    let mut b8 = [0u8; 8];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut b8)?;
    let mut tag = [0u8; TAG_LEN];
    f.seek(SeekFrom::Start(size - 4 - TAG_LEN as u64))?;
    f.read_exact(&mut tag)?;
    Ok((u64::from_le_bytes(b8), tag))
}

/// Read and verify every entry.
pub fn read(data_dir: &Path, key: Option<&Key>) -> Result<Vec<Value>> {
    match key {
        Some(k) => read_sealed(&sealed_path(data_dir), k),
        None => {
            let p = plain_path(data_dir);
            match std::fs::read_to_string(&p) {
                Ok(s) => s.lines().map(|l| serde_json::from_str(l).map_err(AppError::from)).collect(),
                Err(_) => Ok(Vec::new()),
            }
        }
    }
}

fn read_sealed(p: &Path, key: &Key) -> Result<Vec<Value>> {
    let Ok(bytes) = std::fs::read(p) else { return Ok(Vec::new()) };
    if bytes.len() < HEADER as usize || &bytes[..8] != MAGIC {
        return Err(tamper(p, "bad header"));
    }
    let file_id = &bytes[8..HEADER as usize];
    let mut prev = [0u8; TAG_LEN];
    let mut off = HEADER as usize;
    let mut out = Vec::new();
    while off < bytes.len() {
        let expect = out.len() as u64 + 1;
        let rec = bytes.get(off..off + 12).ok_or_else(|| tamper(p, format!("record {expect} truncated")))?;
        let seq = u64::from_le_bytes(rec[..8].try_into().expect("8"));
        let len = u32::from_le_bytes(rec[8..12].try_into().expect("4")) as usize;
        let sealed =
            bytes.get(off + 12..off + 12 + len).ok_or_else(|| tamper(p, format!("record {expect} truncated")))?;
        let trailer = bytes.get(off + 12 + len..off + 16 + len).ok_or_else(|| tamper(p, "truncated"))?;
        if seq != expect || trailer != &rec[8..12] || len < TAG_LEN {
            return Err(tamper(p, format!("record {expect} is missing or out of order")));
        }
        let plain = crypto::open(key, &aad(file_id, seq, &prev), sealed, "audit record")
            .map_err(|_| tamper(p, format!("record {seq} does not authenticate (edited, removed or reordered)")))?;
        out.push(serde_json::from_slice(&plain)?);
        prev.copy_from_slice(&sealed[len - TAG_LEN..]);
        off += 16 + len;
    }
    Ok(out)
}

/// Re-seal a plaintext `audit.jsonl` into the encrypted log (`db encrypt`).
pub fn migrate_plain(data_dir: &Path, key: &Key) -> Result<usize> {
    let old = read(data_dir, None)?;
    for e in &old {
        let mut details = e.get("details").cloned().unwrap_or(Value::Null);
        if let Some(o) = details.as_object_mut() {
            o.insert("migrated_ts".into(), e.get("ts").cloned().unwrap_or(Value::Null));
        }
        append_sealed(&sealed_path(data_dir), key, e["command"].as_str().unwrap_or("?"), details)?;
    }
    let p = plain_path(data_dir);
    if p.exists() {
        crypto::shred(&p)?;
    }
    Ok(old.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let k = Key::random().unwrap();
        for i in 0..3 {
            append(dir.path(), Some(&k), "lookup", json!({"rows": i})).unwrap();
        }
        let v = read(dir.path(), Some(&k)).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[2]["details"]["rows"], 2);
        assert!(read(dir.path(), Some(&Key::random().unwrap())).is_err());
        let p = sealed_path(dir.path());
        let bytes = std::fs::read(&p).unwrap();
        let mut t = bytes.clone();
        let n = t.len();
        t[n - 10] ^= 1;
        std::fs::write(&p, &t).unwrap();
        assert_eq!(read(dir.path(), Some(&k)).unwrap_err().kind, ErrorKind::Crypto);
        // Dropping the first record breaks the chain.
        let first_len = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        let mut t = bytes[..HEADER as usize].to_vec();
        t.extend_from_slice(&bytes[HEADER as usize + 16 + first_len..]);
        std::fs::write(&p, &t).unwrap();
        assert!(read(dir.path(), Some(&k)).is_err());
    }
}
