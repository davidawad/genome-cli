//! Per-kit genotype store: sorted fixed-width site records plus a string heap
//! and an rsid index, in `<data_dir>/kits/<id>/`.
//!
//! fsqlite keeps kit metadata, but inserting millions of genotype rows into it
//! is too slow for a 5M-record WGS VCF (see README, "Storage"). This layout is
//! write-once, sorted by (contig, pos), and supports O(log n) position and
//! rsid lookups with positioned reads, so lookups never load the whole kit.
//!
//! Files:
//! - `sites.bin`: magic `GNMSITE1`, then 32-byte little-endian records
//!   `contig u16 | zygosity u8 | flags u8 | pos u32 | end u32 | rs u32 | qual f32 | depth u32 | heap u64`
//! - `heap.bin`: per record `len u32` + `rsid\tref\talt\tgenotype\tfilter\tgt` (UTF-8)
//! - `rsid.idx`: magic `GNMRSID1`, sorted `rs u32 | record u32` pairs
//! - `contigs.json`: contig names indexed by the `contig` field
//!
//! In an encrypted database every file is a chunked AEAD sealed file
//! (`crypto::SealedWriter`, label = file name) over exactly these bytes, so
//! positioned reads decrypt only the 64 KiB chunks they touch.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::crypto::{Key, Sink, Source};
use crate::error::{AppError, Result};
use crate::model::{primary_index, rs_number, Call, Zygosity};

const SITE_MAGIC: &[u8; 8] = b"GNMSITE1";
const RSID_MAGIC: &[u8; 8] = b"GNMRSID1";
const REC: usize = 32;

const F_QUAL: u8 = 1;
const F_DEPTH: u8 = 2;
const F_REF_BLOCK: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Site {
    pub contig: u16,
    pub zyg: u8,
    pub flags: u8,
    pub pos: u32,
    pub end: u32,
    pub rs: u32,
    pub qual: f32,
    pub depth: u32,
    pub heap: u64,
}

impl Site {
    fn encode(&self) -> [u8; REC] {
        let mut b = [0u8; REC];
        b[0..2].copy_from_slice(&self.contig.to_le_bytes());
        b[2] = self.zyg;
        b[3] = self.flags;
        b[4..8].copy_from_slice(&self.pos.to_le_bytes());
        b[8..12].copy_from_slice(&self.end.to_le_bytes());
        b[12..16].copy_from_slice(&self.rs.to_le_bytes());
        b[16..20].copy_from_slice(&self.qual.to_le_bytes());
        b[20..24].copy_from_slice(&self.depth.to_le_bytes());
        b[24..32].copy_from_slice(&self.heap.to_le_bytes());
        b
    }
    fn decode(b: &[u8]) -> Self {
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"));
        Self {
            contig: u16::from_le_bytes([b[0], b[1]]),
            zyg: b[2],
            flags: b[3],
            pos: u32_at(4),
            end: u32_at(8),
            rs: u32_at(12),
            qual: f32::from_le_bytes(b[16..20].try_into().expect("4 bytes")),
            depth: u32_at(20),
            heap: u64::from_le_bytes(b[24..32].try_into().expect("8 bytes")),
        }
    }
    pub fn zygosity(&self) -> Zygosity {
        Zygosity::from_code(self.zyg)
    }
    pub fn is_ref_block(&self) -> bool {
        self.flags & F_REF_BLOCK != 0
    }
}

fn store_err(p: &Path, e: impl std::fmt::Display) -> AppError {
    AppError::io(format!("{}: {e}", p.display()))
}

/// Accumulates calls during import, then sorts and writes the kit files.
pub struct Writer {
    dir: PathBuf,
    key: Option<Key>,
    heap: Sink,
    heap_len: u64,
    pub sites: Vec<Site>,
    pub contigs: Vec<String>,
    contig_ids: std::collections::HashMap<String, u16>,
}

impl Writer {
    /// Create a store in `dir`, sealed with `key` (plaintext without one).
    pub fn create(dir: &Path, key: Option<&Key>) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| store_err(dir, e))?;
        let heap = Sink::create(&dir.join("heap.bin"), key, "heap.bin")?;
        // Contig ids 0..=25 are reserved for the primary chromosomes so the sort
        // order is 1..22, X, Y, MT, then other contigs in first-seen order.
        let mut contigs: Vec<String> = vec![String::new(); 26];
        (1..=22).for_each(|i| contigs[i] = i.to_string());
        contigs[23] = "X".into();
        contigs[24] = "Y".into();
        contigs[25] = "MT".into();
        let contig_ids = contigs.iter().enumerate().skip(1).map(|(i, c)| (c.clone(), i as u16)).collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            key: key.cloned(),
            heap,
            heap_len: 0,
            sites: Vec::new(),
            contigs,
            contig_ids,
        })
    }

    fn contig_id(&mut self, chrom: &str) -> Result<u16> {
        if let Some(i) = primary_index(chrom) {
            return Ok(i);
        }
        if let Some(i) = self.contig_ids.get(chrom) {
            return Ok(*i);
        }
        let id = u16::try_from(self.contigs.len()).map_err(|_| AppError::invalid("more than 65535 contigs"))?;
        self.contigs.push(chrom.to_string());
        self.contig_ids.insert(chrom.to_string(), id);
        Ok(id)
    }

    pub fn push(&mut self, c: &Call) -> Result<()> {
        let contig = self.contig_id(&c.chrom)?;
        let payload = format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            c.rsid.as_deref().unwrap_or(""),
            c.reference.as_deref().unwrap_or(""),
            c.alt.join(","),
            c.genotype,
            c.filter.as_deref().unwrap_or(""),
            c.gt.as_deref().unwrap_or("")
        );
        let heap = self.heap_len;
        self.heap.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.heap.write_all(payload.as_bytes())?;
        self.heap_len += 4 + payload.len() as u64;
        let flags = (if c.quality.is_some() { F_QUAL } else { 0 })
            | (if c.depth.is_some() { F_DEPTH } else { 0 })
            | (if c.ref_block { F_REF_BLOCK } else { 0 });
        self.sites.push(Site {
            contig,
            zyg: c.zyg().code(),
            flags,
            pos: c.pos,
            end: c.end.max(c.pos),
            rs: c.rsid.as_deref().and_then(rs_number).unwrap_or(0),
            qual: c.quality.unwrap_or(0.0),
            depth: c.depth.unwrap_or(0),
            heap,
        });
        Ok(())
    }

    /// Sort, write `sites.bin`, `rsid.idx`, `contigs.json`. Returns the sorted sites.
    pub fn finish(mut self) -> Result<(Vec<Site>, Vec<String>)> {
        self.heap.finish()?;
        let key = self.key.as_ref();
        self.sites.sort_by_key(|s| (s.contig, s.pos, s.end));
        let mut w = Sink::create(&self.dir.join("sites.bin"), key, "sites.bin")?;
        w.write_all(SITE_MAGIC)?;
        for s in &self.sites {
            w.write_all(&s.encode())?;
        }
        w.finish()?;
        let mut idx: Vec<(u32, u32)> =
            self.sites.iter().enumerate().filter(|(_, s)| s.rs != 0).map(|(i, s)| (s.rs, i as u32)).collect();
        idx.sort_unstable();
        let mut w = Sink::create(&self.dir.join("rsid.idx"), key, "rsid.idx")?;
        w.write_all(RSID_MAGIC)?;
        for (rs, i) in idx {
            w.write_all(&rs.to_le_bytes())?;
            w.write_all(&i.to_le_bytes())?;
        }
        w.finish()?;
        let mut w = Sink::create(&self.dir.join("contigs.json"), key, "contigs.json")?;
        w.write_all(serde_json::to_string(&self.contigs)?.as_bytes())?;
        w.finish()?;
        Ok((self.sites, self.contigs))
    }
}

/// The files of a kit store (all sealed in an encrypted database).
pub const FILES: &[&str] = &["sites.bin", "heap.bin", "rsid.idx", "contigs.json"];

/// Read access to a kit's genotype store.
pub struct Reader {
    dir: PathBuf,
    key: Option<Key>,
    sites: Source,
    heap: Source,
    rsid: Option<Source>,
    pub n: u64,
    pub contigs: Vec<String>,
}

impl Reader {
    /// Open a store; sealed stores need `key`, and with a key plaintext files are refused.
    pub fn open(dir: &Path, key: Option<&Key>) -> Result<Self> {
        let sp = dir.join("sites.bin");
        let mut sites = Source::open(&sp, key, "sites.bin")?;
        let mut magic = [0u8; 8];
        if sites.len() < 8 || sites.read_at(0, &mut magic).is_err() || &magic != SITE_MAGIC {
            return Err(AppError::invalid(format!("{} is not a genome-cli site store", sp.display())));
        }
        let n = (sites.len() - 8) / REC as u64;
        let heap = Source::open(&dir.join("heap.bin"), key, "heap.bin")?;
        let cp = dir.join("contigs.json");
        let contigs: Vec<String> = serde_json::from_slice(&Source::open(&cp, key, "contigs.json")?.read_all()?)
            .map_err(|e| store_err(&cp, e))?;
        Ok(Self { dir: dir.to_path_buf(), key: key.cloned(), sites, heap, rsid: None, n, contigs })
    }

    pub fn site(&mut self, i: u64) -> Result<Site> {
        let mut b = [0u8; REC];
        self.sites.read_at(8 + i * REC as u64, &mut b)?;
        Ok(Site::decode(&b))
    }

    pub fn contig_id(&self, chrom: &str) -> Option<u16> {
        primary_index(chrom).or_else(|| self.contigs.iter().position(|c| c == chrom).map(|i| i as u16))
    }

    /// Expand a site into a full call.
    pub fn call(&mut self, s: &Site) -> Result<Call> {
        let mut len = [0u8; 4];
        self.heap.read_at(s.heap, &mut len)?;
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        self.heap.read_at(s.heap + 4, &mut buf)?;
        Ok(call_from(s, &self.contigs, &buf))
    }

    /// First index whose (contig, pos) >= key.
    fn lower_bound(&mut self, contig: u16, pos: u32) -> Result<u64> {
        let (mut lo, mut hi) = (0u64, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let s = self.site(mid)?;
            if (s.contig, s.pos) < (contig, pos) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Calls starting at `chrom:pos`, or else a record (gVCF block, deletion) spanning it.
    pub fn at(&mut self, chrom: &str, pos: u32) -> Result<Vec<Call>> {
        let Some(cid) = self.contig_id(chrom) else { return Ok(Vec::new()) };
        let start = self.lower_bound(cid, pos)?;
        let mut out = Vec::new();
        let mut i = start;
        while i < self.n {
            let s = self.site(i)?;
            if s.contig != cid || s.pos != pos {
                break;
            }
            out.push(self.call(&s)?);
            i += 1;
        }
        if out.is_empty() {
            // Look back a little for a spanning record (reference block or long REF).
            let mut j = start;
            let mut steps = 0;
            while j > 0 && steps < 64 {
                j -= 1;
                steps += 1;
                let s = self.site(j)?;
                if s.contig != cid {
                    break;
                }
                if s.end >= pos {
                    out.push(self.call(&s)?);
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Calls for an rsid (via `rsid.idx`).
    pub fn by_rsid(&mut self, rsid: &str) -> Result<Vec<Call>> {
        let Some(rs) = rs_number(rsid) else { return self.scan_rsid(rsid) };
        if self.rsid.is_none() {
            self.rsid = Some(Source::open(&self.dir.join("rsid.idx"), self.key.as_ref(), "rsid.idx")?);
        }
        let mut f = self.rsid.take().expect("opened");
        let res = self.by_rs(&mut f, rs);
        self.rsid = Some(f);
        res
    }

    fn by_rs(&mut self, f: &mut Source, rs: u32) -> Result<Vec<Call>> {
        let n = f.len().saturating_sub(8) / 8;
        let entry = |f: &mut Source, i: u64| -> Result<(u32, u32)> {
            let mut b = [0u8; 8];
            f.read_at(8 + i * 8, &mut b)?;
            Ok((u32::from_le_bytes(b[0..4].try_into().expect("4")), u32::from_le_bytes(b[4..8].try_into().expect("4"))))
        };
        let (mut lo, mut hi) = (0u64, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if entry(f, mid)?.0 < rs {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut out = Vec::new();
        while lo < n {
            let (r, i) = entry(f, lo)?;
            if r != rs {
                break;
            }
            let s = self.site(u64::from(i))?;
            out.push(self.call(&s)?);
            lo += 1;
        }
        Ok(out)
    }

    /// Linear scan for non-numeric ids (e.g. 23andMe internal `i5000001`).
    fn scan_rsid(&mut self, id: &str) -> Result<Vec<Call>> {
        let mut out = Vec::new();
        self.for_each(&mut |c| {
            if c.rsid.as_deref() == Some(id) {
                out.push(c);
            }
            Ok(true)
        })?;
        Ok(out)
    }

    /// Stream all calls in sorted order. The callback returns false to stop.
    pub fn for_each(&mut self, f: &mut dyn FnMut(Call) -> Result<bool>) -> Result<()> {
        for i in 0..self.n {
            let s = self.site(i)?;
            if !f(self.call(&s)?)? {
                break;
            }
        }
        Ok(())
    }

    /// Stream calls overlapping a region.
    pub fn for_region(
        &mut self,
        chrom: &str,
        start: u32,
        end: u32,
        f: &mut dyn FnMut(Call) -> Result<()>,
    ) -> Result<()> {
        let Some(cid) = self.contig_id(chrom) else { return Ok(()) };
        let mut i = self.lower_bound(cid, start)?;
        while i < self.n {
            let s = self.site(i)?;
            if s.contig != cid || s.pos > end {
                break;
            }
            let c = self.call(&s)?;
            f(c)?;
            i += 1;
        }
        Ok(())
    }
}

fn call_from(s: &Site, contigs: &[String], payload: &[u8]) -> Call {
    let text = String::from_utf8_lossy(payload);
    let mut parts = text.split('\t');
    let mut next = || parts.next().unwrap_or("").to_string();
    let (rsid, reference, alt, genotype, filter, gt) = (next(), next(), next(), next(), next(), next());
    let some = |v: String| Some(v).filter(|v| !v.is_empty());
    Call {
        chrom: contigs.get(s.contig as usize).cloned().unwrap_or_default(),
        pos: s.pos,
        end: s.end,
        rsid: some(rsid),
        reference: some(reference),
        alt: if alt.is_empty() { Vec::new() } else { alt.split(',').map(str::to_string).collect() },
        genotype,
        gt: some(gt),
        zygosity: Some(s.zygosity()),
        filter: some(filter),
        quality: (s.flags & F_QUAL != 0).then_some(s.qual),
        depth: (s.flags & F_DEPTH != 0).then_some(s.depth),
        ref_block: s.is_ref_block(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(chrom: &str, pos: u32, rsid: Option<&str>, g: &str) -> Call {
        Call {
            chrom: chrom.into(),
            pos,
            end: pos,
            rsid: rsid.map(str::to_string),
            reference: Some("A".into()),
            alt: vec!["G".into()],
            genotype: g.into(),
            zygosity: Some(Zygosity::Het),
            quality: Some(30.5),
            ..Call::default()
        }
    }

    #[test]
    fn roundtrip_sorted_lookup() {
        roundtrip(None);
        roundtrip(Some(Key::random().unwrap()));
    }

    fn roundtrip(key: Option<Key>) {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::create(dir.path(), key.as_ref()).unwrap();
        for c in [
            call("X", 50, Some("rs9"), "AG"),
            call("2", 10, Some("rs7"), "AG"),
            call("1_KI270706v1_random", 5, None, "AG"),
            call("1", 300, Some("rs8"), "AA"),
            call("1", 20, None, "GG"),
        ] {
            w.push(&c).unwrap();
        }
        let mut block = call("3", 100, None, "AA");
        block.end = 200;
        block.ref_block = true;
        w.push(&block).unwrap();
        w.finish().unwrap();
        let mut r = Reader::open(dir.path(), key.as_ref()).unwrap();
        assert_eq!(r.n, 6);
        let mut order = Vec::new();
        r.for_each(&mut |c| {
            order.push(format!("{}:{}", c.chrom, c.pos));
            Ok(true)
        })
        .unwrap();
        assert_eq!(order, ["1:20", "1:300", "2:10", "3:100", "X:50", "1_KI270706v1_random:5"]);
        assert_eq!(r.at("1", 300).unwrap()[0].genotype, "AA");
        assert_eq!(r.at("1", 301).unwrap().len(), 0);
        assert_eq!(r.by_rsid("rs7").unwrap()[0].pos, 10);
        assert!(r.by_rsid("rs1").unwrap().is_empty());
        let spanning = r.at("3", 150).unwrap();
        assert!(spanning[0].ref_block);
        assert_eq!(r.at("1_KI270706v1_random", 5).unwrap()[0].quality, Some(30.5));
    }
}
