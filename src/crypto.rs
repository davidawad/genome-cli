//! Encryption at rest: XChaCha20-Poly1305 AEAD, Argon2id key derivation and a
//! chunked, random-access sealed file format. See docs/security.md.
//!
//! Sealed file (`GNMSEAL1`), used for genotype stores, sealed exports and
//! pipeline outputs:
//!
//! ```text
//! header (32 bytes): magic "GNMSEAL1" | chunk_size u32 | version u32 | file_id [16]
//! chunk i:           nonce [24] | ciphertext (chunk_size bytes, last chunk 0..=chunk_size) | tag [16]
//! ```
//!
//! Every chunk is authenticated with AAD = header | label | i (u64) | final (u8),
//! so chunks cannot be modified, reordered, moved between files or between
//! roles (`label`, e.g. `sites.bin` vs `heap.bin`), and truncation at a chunk
//! boundary is detected because the new last chunk was not sealed as final.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::error::{AppError, Result};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
pub const SEAL_MAGIC: &[u8; 8] = b"GNMSEAL1";
const SEAL_VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
pub const CHUNK: usize = 64 * 1024;

/// A 256-bit symmetric key, wiped from memory on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    pub fn random() -> Result<Self> {
        let mut k = [0u8; KEY_LEN];
        fill_random(&mut k)?;
        Ok(Self(k))
    }
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let arr: [u8; KEY_LEN] = b.try_into().map_err(|_| AppError::invalid("key material has the wrong length"))?;
        Ok(Self(arr))
    }
    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.0.as_slice().into())
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

pub fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| AppError::new(crate::error::ErrorKind::General, format!("OS RNG: {e}")))
}

/// The error for any failed authentication: never distinguishes wrong key from tampering.
pub fn auth_error(what: &str) -> AppError {
    AppError::new(
        crate::error::ErrorKind::Crypto,
        format!("{what}: decryption failed (wrong key, or the file was modified, truncated or corrupted)"),
    )
}

/// Seal `plaintext` with a fresh random nonce: `nonce | ciphertext | tag`.
pub fn seal(key: &Key, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    let ct = key
        .cipher()
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .map_err(|_| AppError::invalid("encryption failed"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a `seal` output. `what` names the object in the error message.
pub fn open(key: &Key, aad: &[u8], sealed: &[u8], what: &str) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(auth_error(what));
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    key.cipher()
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| auth_error(what))
}

// ---------------------------------------------------------------------------
// Argon2id
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m: u32,
    /// Iterations.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl KdfParams {
    /// OWASP-recommended Argon2id profile (64 MiB, 3 passes).
    pub const DEFAULT: Self = Self { m: 64 * 1024, t: 3, p: 1 };
    /// Deliberately weak parameters for test suites (`GENOME_INSECURE_FAST_KDF=1`).
    pub const FAST_INSECURE: Self = Self { m: 1024, t: 1, p: 1 };

    pub fn for_new_key() -> Self {
        if std::env::var_os("GENOME_INSECURE_FAST_KDF").is_some_and(|v| v == "1") {
            Self::FAST_INSECURE
        } else {
            Self::DEFAULT
        }
    }
}

pub fn derive_key(passphrase: &[u8], salt: &[u8], p: KdfParams) -> Result<Key> {
    let params = argon2::Params::new(p.m, p.t, p.p, Some(KEY_LEN))
        .map_err(|e| AppError::invalid(format!("invalid Argon2id parameters: {e}")))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    a.hash_password_into(passphrase, salt, &mut out).map_err(|e| AppError::invalid(format!("Argon2id: {e}")))?;
    let k = Key(out);
    out.zeroize();
    Ok(k)
}

// ---------------------------------------------------------------------------
// Hex helpers (envelopes are JSON)
// ---------------------------------------------------------------------------

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(AppError::invalid("bad hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| AppError::invalid("bad hex")))
        .collect()
}

// ---------------------------------------------------------------------------
// Sealed files
// ---------------------------------------------------------------------------

fn chunk_aad(header: &[u8], label: &str, i: u64, last: bool) -> Vec<u8> {
    let mut aad = Vec::with_capacity(header.len() + label.len() + 9);
    aad.extend_from_slice(header);
    aad.extend_from_slice(label.as_bytes());
    aad.extend_from_slice(&i.to_le_bytes());
    aad.push(u8::from(last));
    aad
}

/// Does `path` start with the sealed-file magic?
pub fn is_sealed_file(path: &Path) -> bool {
    let mut m = [0u8; 8];
    File::open(path).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == SEAL_MAGIC
}

/// Streaming writer for a sealed file. Writes to `<path>.tmp` and renames into
/// place (after fsync) in [`SealedWriter::finish`]; dropping it unfinished
/// removes the temp file.
pub struct SealedWriter {
    key: Key,
    label: String,
    header: [u8; HEADER_LEN],
    out: Option<std::io::BufWriter<File>>,
    tmp: PathBuf,
    dest: PathBuf,
    buf: Zeroizing<Vec<u8>>,
    index: u64,
    len: u64,
}

impl SealedWriter {
    pub fn create(path: &Path, key: &Key, label: &str) -> Result<Self> {
        let tmp = tmp_path(path);
        let f = File::create(&tmp).map_err(|e| AppError::io(format!("{}: {e}", tmp.display())))?;
        let mut header = [0u8; HEADER_LEN];
        header[0..8].copy_from_slice(SEAL_MAGIC);
        header[8..12].copy_from_slice(&(CHUNK as u32).to_le_bytes());
        header[12..16].copy_from_slice(&SEAL_VERSION.to_le_bytes());
        fill_random(&mut header[16..32])?;
        let mut out = std::io::BufWriter::with_capacity(4 * CHUNK, f);
        out.write_all(&header)?;
        Ok(Self {
            key: key.clone(),
            label: label.to_string(),
            header,
            out: Some(out),
            tmp,
            dest: path.to_path_buf(),
            buf: Zeroizing::new(Vec::with_capacity(CHUNK)),
            index: 0,
            len: 0,
        })
    }

    fn emit(&mut self, last: bool) -> std::io::Result<()> {
        let aad = chunk_aad(&self.header, &self.label, self.index, last);
        let sealed = seal(&self.key, &aad, &self.buf).map_err(|e| std::io::Error::other(e.message))?;
        self.out.as_mut().expect("open until finish").write_all(&sealed)?;
        self.buf.clear();
        self.index += 1;
        Ok(())
    }

    /// Plaintext bytes written so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Seal the final chunk, fsync and atomically rename into place.
    pub fn finish(mut self) -> Result<()> {
        self.emit(true)?;
        let f = self.out.take().expect("open until finish").into_inner().map_err(|e| AppError::io(e.to_string()))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.tmp, &self.dest)
            .map_err(|e| AppError::io(format!("renaming into {}: {e}", self.dest.display())))?;
        sync_parent(&self.dest);
        Ok(())
    }
}

impl Write for SealedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut rest = data;
        while !rest.is_empty() {
            // A full chunk is only emitted once more data arrives, so the last
            // chunk is always sealed as final in `finish` (even when full).
            if self.buf.len() == CHUNK {
                self.emit(false)?;
            }
            let n = (CHUNK - self.buf.len()).min(rest.len());
            self.buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        self.len += data.len() as u64;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for SealedWriter {
    fn drop(&mut self) {
        if self.out.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

/// Best-effort fsync of the directory holding `path` (makes a rename durable).
pub fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

/// Random-access reader for a sealed file. Decrypted chunks are cached (a few
/// at a time) so binary searches and sequential scans stay fast.
pub struct SealedReader {
    file: File,
    path: PathBuf,
    key: Key,
    label: String,
    header: [u8; HEADER_LEN],
    chunk: usize,
    nchunks: u64,
    len: u64,
    cache: Vec<(u64, Zeroizing<Vec<u8>>)>,
}

const CACHE_SLOTS: usize = 4;

impl SealedReader {
    pub fn open(path: &Path, key: &Key, label: &str) -> Result<Self> {
        let mut file = File::open(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
        let mut header = [0u8; HEADER_LEN];
        let size = file.metadata()?.len();
        let what = path.display().to_string();
        if size < HEADER_LEN as u64 || file.read_exact(&mut header).is_err() || &header[0..8] != SEAL_MAGIC {
            return Err(AppError::invalid(format!("{what} is not a genome-cli sealed file")));
        }
        let chunk = u32::from_le_bytes(header[8..12].try_into().expect("4")) as usize;
        let version = u32::from_le_bytes(header[12..16].try_into().expect("4"));
        if version != SEAL_VERSION || chunk == 0 || chunk > 64 * 1024 * 1024 {
            return Err(AppError::invalid(format!("{what}: unsupported sealed file version {version}")));
        }
        let rec = (chunk + NONCE_LEN + TAG_LEN) as u64;
        let body = size - HEADER_LEN as u64;
        let nchunks = body.div_ceil(rec).max(1);
        let last = body.saturating_sub((nchunks - 1) * rec);
        if last < (NONCE_LEN + TAG_LEN) as u64 {
            return Err(auth_error(&what));
        }
        let len = (nchunks - 1) * chunk as u64 + last - (NONCE_LEN + TAG_LEN) as u64;
        let mut r = Self {
            file,
            path: path.to_path_buf(),
            key: key.clone(),
            label: label.to_string(),
            header,
            chunk,
            nchunks,
            len,
            cache: Vec::new(),
        };
        // Authenticates the key, the header and the end of the file up front.
        r.load(nchunks - 1)?;
        Ok(r)
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn load(&mut self, i: u64) -> Result<usize> {
        if let Some(pos) = self.cache.iter().position(|(c, _)| *c == i) {
            return Ok(pos);
        }
        let rec = (self.chunk + NONCE_LEN + TAG_LEN) as u64;
        let off = HEADER_LEN as u64 + i * rec;
        let last = i + 1 == self.nchunks;
        let size = if last { (self.len - i * self.chunk as u64) as usize } else { self.chunk } + NONCE_LEN + TAG_LEN;
        let mut buf = vec![0u8; size];
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(&mut buf).map_err(|_| auth_error(&self.path.display().to_string()))?;
        let aad = chunk_aad(&self.header, &self.label, i, last);
        let plain = open(&self.key, &aad, &buf, &self.path.display().to_string())?;
        if self.cache.len() >= CACHE_SLOTS {
            self.cache.remove(0);
        }
        self.cache.push((i, plain));
        Ok(self.cache.len() - 1)
    }

    /// Fill `buf` from logical offset `off`.
    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off + buf.len() as u64 > self.len {
            return Err(AppError::invalid(format!("{}: read past end of sealed file", self.path.display())));
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let ci = pos / self.chunk as u64;
            let within = (pos % self.chunk as u64) as usize;
            let slot = self.load(ci)?;
            let data = &self.cache[slot].1;
            let n = (data.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&data[within..within + n]);
            done += n;
        }
        Ok(())
    }

    /// Decrypt the whole file into memory.
    pub fn read_all(&mut self) -> Result<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(vec![0u8; self.len as usize]);
        self.read_at(0, &mut out)?;
        Ok(out)
    }
}

/// Positioned byte source over a plain or sealed file, with block caching.
pub enum Source {
    Plain { file: File, len: u64, block: Option<(u64, Vec<u8>)> },
    Sealed(Box<SealedReader>),
}

const PLAIN_BLOCK: u64 = 64 * 1024;

impl Source {
    /// Open `path`. Sealed files need `key`; with a key, a plaintext file is refused.
    pub fn open(path: &Path, key: Option<&Key>, label: &str) -> Result<Self> {
        match (is_sealed_file(path), key) {
            (true, Some(k)) => Ok(Self::Sealed(Box::new(SealedReader::open(path, k, label)?))),
            (true, None) => Err(AppError::new(
                crate::error::ErrorKind::Crypto,
                format!("{} is encrypted but no key is available", path.display()),
            )),
            (false, Some(_)) => Err(AppError::new(
                crate::error::ErrorKind::Crypto,
                format!(
                    "{} is plaintext inside an encrypted database (run `genome db encrypt` to seal it)",
                    path.display()
                ),
            )),
            (false, None) => {
                let file = File::open(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
                let len = file.metadata()?.len();
                Ok(Self::Plain { file, len, block: None })
            }
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Plain { len, .. } => *len,
            Self::Sealed(r) => r.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Sealed(r) => r.read_at(off, buf),
            Self::Plain { file, len, block } => {
                if off + buf.len() as u64 > *len {
                    return Err(AppError::io("read past end of file"));
                }
                let mut done = 0;
                while done < buf.len() {
                    let pos = off + done as u64;
                    let start = pos - pos % PLAIN_BLOCK;
                    if block.as_ref().is_none_or(|(s, _)| *s != start) {
                        let n = PLAIN_BLOCK.min(*len - start) as usize;
                        let mut b = vec![0u8; n];
                        file.seek(SeekFrom::Start(start))?;
                        file.read_exact(&mut b)?;
                        *block = Some((start, b));
                    }
                    let data = &block.as_ref().expect("loaded").1;
                    let within = (pos - start) as usize;
                    let n = (data.len() - within).min(buf.len() - done);
                    buf[done..done + n].copy_from_slice(&data[within..within + n]);
                    done += n;
                }
                Ok(())
            }
        }
    }

    pub fn read_all(&mut self) -> Result<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(vec![0u8; self.len() as usize]);
        self.read_at(0, &mut out)?;
        Ok(out)
    }
}

/// A finishing writer: plain (buffered file) or sealed.
pub enum Sink {
    Plain(std::io::BufWriter<File>),
    Sealed(Box<SealedWriter>),
}

impl Sink {
    pub fn create(path: &Path, key: Option<&Key>, label: &str) -> Result<Self> {
        Ok(match key {
            Some(k) => Self::Sealed(Box::new(SealedWriter::create(path, k, label)?)),
            None => Self::Plain(std::io::BufWriter::with_capacity(
                1 << 20,
                File::create(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?,
            )),
        })
    }
    pub fn finish(self) -> Result<()> {
        match self {
            Self::Plain(mut w) => w.flush().map_err(AppError::from),
            Self::Sealed(w) => w.finish(),
        }
    }
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(w) => w.write(b),
            Self::Sealed(w) => w.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(w) => w.flush(),
            Self::Sealed(w) => w.flush(),
        }
    }
}

/// Seal an existing plaintext file into `dest` (streaming).
pub fn seal_file(src: &Path, dest: &Path, key: &Key, label: &str) -> Result<()> {
    let mut r = File::open(src).map_err(|e| AppError::io(format!("{}: {e}", src.display())))?;
    let mut w = SealedWriter::create(dest, key, label)?;
    let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    w.finish()
}

/// Best-effort secure delete: overwrite the file's bytes with random data,
/// fsync, then unlink. On SSDs, copy-on-write and journaling filesystems the
/// old blocks may survive (see docs/security.md); this is "secure-ish".
pub fn shred(path: &Path) -> Result<()> {
    let Ok(meta) = std::fs::metadata(path) else { return Ok(()) };
    if meta.is_file() {
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        let mut left = meta.len();
        let mut buf = vec![0u8; CHUNK];
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            fill_random(&mut buf[..n])?;
            f.write_all(&buf[..n])?;
            left -= n as u64;
        }
        f.sync_all()?;
    }
    std::fs::remove_file(path).map_err(|e| AppError::io(format!("removing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sealed(path: &Path, key: &Key, data: &[u8]) {
        let mut w = SealedWriter::create(path, key, "t").unwrap();
        // Uneven writes cross chunk boundaries.
        for part in data.chunks(7919) {
            w.write_all(part).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn sealed_roundtrip_random_access() {
        let dir = tempfile::tempdir().unwrap();
        let key = Key::random().unwrap();
        for len in [0usize, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK, 3 * CHUNK + 17] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let p = dir.path().join(format!("f{len}"));
            write_sealed(&p, &key, &data);
            let mut r = SealedReader::open(&p, &key, "t").unwrap();
            assert_eq!(r.len(), len as u64);
            assert_eq!(&r.read_all().unwrap()[..], &data[..]);
            if len > 10 {
                let mut b = [0u8; 10];
                r.read_at(len as u64 - 10, &mut b).unwrap();
                assert_eq!(&b, &data[len - 10..]);
            }
        }
    }

    #[test]
    fn wrong_key_label_tamper_truncation_fail() {
        let dir = tempfile::tempdir().unwrap();
        let key = Key::random().unwrap();
        let p = dir.path().join("f");
        let data = vec![7u8; 2 * CHUNK + 5];
        write_sealed(&p, &key, &data);
        assert!(SealedReader::open(&p, &Key::random().unwrap(), "t").is_err());
        assert!(SealedReader::open(&p, &key, "other").is_err());
        let bytes = std::fs::read(&p).unwrap();
        // Flip a byte in the first chunk: open succeeds (last chunk ok), read fails.
        let mut t = bytes.clone();
        t[HEADER_LEN + NONCE_LEN + 3] ^= 1;
        std::fs::write(&p, &t).unwrap();
        let mut r = SealedReader::open(&p, &key, "t").unwrap();
        let e = r.read_at(0, &mut [0u8; 4]).unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::Crypto);
        // Header tamper fails at open.
        let mut t = bytes.clone();
        t[20] ^= 1;
        std::fs::write(&p, &t).unwrap();
        assert!(SealedReader::open(&p, &key, "t").is_err());
        // Truncation at a chunk boundary is detected.
        let rec = CHUNK + NONCE_LEN + TAG_LEN;
        std::fs::write(&p, &bytes[..HEADER_LEN + 2 * rec]).unwrap();
        assert!(SealedReader::open(&p, &key, "t").is_err());
    }

    #[test]
    fn seal_open_and_kdf() {
        let k = derive_key(b"pw", b"0123456789abcdef", KdfParams::FAST_INSECURE).unwrap();
        let k2 = derive_key(b"pw", b"0123456789abcdef", KdfParams::FAST_INSECURE).unwrap();
        assert_eq!(k.bytes(), k2.bytes());
        let s = seal(&k, b"aad", b"hello").unwrap();
        assert_eq!(&open(&k2, b"aad", &s, "x").unwrap()[..], b"hello");
        assert!(open(&k, b"other", &s, "x").is_err());
        assert_eq!(unhex(&hex(&[0, 255, 16])).unwrap(), vec![0, 255, 16]);
    }
}
