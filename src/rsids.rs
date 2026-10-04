//! rsid <-> coordinate resolution: the bundled curated table plus an optional
//! dbSNP index built from a user-supplied dbSNP VCF (`genome rsid-table import`).

use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, Result};
use crate::model::{normalize_chrom, primary_index, rs_number, Build};

pub const CURATED_TSV: &str = include_str!("../data/rsid_table.tsv");

#[derive(Debug, Clone, Serialize)]
pub struct CuratedRow {
    pub rsid: String,
    pub gene: String,
    pub chrom: String,
    pub grch37_pos: u32,
    pub grch37_ref: String,
    pub grch37_alt: Vec<String>,
    pub grch38_pos: u32,
    pub grch38_ref: String,
    pub grch38_alt: Vec<String>,
    pub source: String,
}

pub fn curated() -> Vec<CuratedRow> {
    CURATED_TSV
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("rsid\t") && !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let alts = |s: &str| s.split(',').map(str::to_string).collect::<Vec<_>>();
            (f.len() >= 10).then(|| CuratedRow {
                rsid: f[0].into(),
                gene: f[1].into(),
                chrom: f[2].into(),
                grch37_pos: f[3].parse().unwrap_or(0),
                grch37_ref: f[4].into(),
                grch37_alt: alts(f[5]),
                grch38_pos: f[6].parse().unwrap_or(0),
                grch38_ref: f[7].into(),
                grch38_alt: alts(f[8]),
                source: f[9].into(),
            })
        })
        .collect()
}

/// A resolved coordinate in one build.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coord {
    pub rsid: String,
    pub chrom: String,
    pub pos: u32,
    pub reference: Option<String>,
    pub alt: Vec<String>,
    /// curated | dbsnp
    pub source: &'static str,
}

// ---------------------------------------------------------------------------
// dbSNP index: 12-byte records `rs u32 | pos u32 | contig u8 | ref u8 | altmask u8 | 0`
// in two files, sorted by rsid and by (contig, pos).
// ---------------------------------------------------------------------------

const DB_MAGIC: &[u8; 8] = b"GNMDBSN1";
const DREC: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DbRec {
    rs: u32,
    contig: u8,
    pos: u32,
    reference: u8,
    alts: u8,
}

const BASES: [char; 4] = ['A', 'C', 'G', 'T'];

fn base_code(s: &str) -> u8 {
    match s {
        "A" => 1,
        "C" => 2,
        "G" => 3,
        "T" => 4,
        _ => 0,
    }
}

impl DbRec {
    fn encode(&self) -> [u8; DREC] {
        let mut b = [0u8; DREC];
        b[0..4].copy_from_slice(&self.rs.to_le_bytes());
        b[4..8].copy_from_slice(&self.pos.to_le_bytes());
        b[8] = self.contig;
        b[9] = self.reference;
        b[10] = self.alts;
        b
    }
    fn decode(b: &[u8]) -> Self {
        Self {
            rs: u32::from_le_bytes(b[0..4].try_into().expect("4")),
            pos: u32::from_le_bytes(b[4..8].try_into().expect("4")),
            contig: b[8],
            reference: b[9],
            alts: b[10],
        }
    }
    fn pos_key(&self) -> (u8, u32, u32) {
        (self.contig, self.pos, self.rs)
    }
    fn coord(&self) -> Coord {
        let contig = match self.contig {
            23 => "X".to_string(),
            24 => "Y".to_string(),
            25 => "MT".to_string(),
            n => n.to_string(),
        };
        Coord {
            rsid: format!("rs{}", self.rs),
            chrom: contig,
            pos: self.pos,
            reference: (1..=4).contains(&self.reference).then(|| BASES[self.reference as usize - 1].to_string()),
            alt: (0..4).filter(|i| self.alts & (1 << i) != 0).map(|i| BASES[i].to_string()).collect(),
            source: "dbsnp",
        }
    }
}

pub fn dbsnp_paths(cache_dir: &Path, build: Build) -> (PathBuf, PathBuf) {
    let d = cache_dir.join("dbsnp");
    (d.join(format!("{}.by-rsid.bin", build.as_str())), d.join(format!("{}.by-pos.bin", build.as_str())))
}

fn write_run(recs: &mut [DbRec], by_pos: bool, path: &Path) -> Result<()> {
    if by_pos {
        recs.sort_unstable_by_key(DbRec::pos_key);
    } else {
        recs.sort_unstable();
    }
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    for r in recs.iter() {
        w.write_all(&r.encode())?;
    }
    w.flush()?;
    Ok(())
}

/// k-way merge of sorted run files into `dest` (with magic header).
fn merge_runs(runs: &[PathBuf], by_pos: bool, dest: &Path) -> Result<u64> {
    let mut readers: Vec<BufReader<File>> = runs
        .iter()
        .map(|p| File::open(p).map(|f| BufReader::with_capacity(1 << 20, f)))
        .collect::<std::io::Result<_>>()?;
    let next = |r: &mut BufReader<File>| -> Option<DbRec> {
        let mut b = [0u8; DREC];
        r.read_exact(&mut b).ok().map(|()| DbRec::decode(&b))
    };
    let key = |r: &DbRec| {
        if by_pos {
            (r.contig as u64, r.pos as u64, r.rs as u64)
        } else {
            (r.rs as u64, r.contig as u64, r.pos as u64)
        }
    };
    let mut heap = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(rec) = next(r) {
            heap.push(std::cmp::Reverse((key(&rec), i, rec.encode())));
        }
    }
    let tmp = dest.with_extension("part");
    let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
    w.write_all(DB_MAGIC)?;
    let mut n = 0;
    while let Some(std::cmp::Reverse((_, i, bytes))) = heap.pop() {
        w.write_all(&bytes)?;
        n += 1;
        if let Some(rec) = next(&mut readers[i]) {
            heap.push(std::cmp::Reverse((key(&rec), i, rec.encode())));
        }
    }
    w.flush()?;
    std::fs::rename(&tmp, dest)?;
    Ok(n)
}

/// Build the dbSNP index for `build` from a dbSNP VCF. Returns (records indexed, records skipped).
pub fn import_dbsnp(
    vcf: &Path,
    cache_dir: &Path,
    build: Option<Build>,
    chunk: usize,
    progress: &dyn Fn(&str),
) -> Result<(Build, u64, u64)> {
    let mut reader = crate::parse::open_text(vcf)?;
    let mut line = String::new();
    let mut header = String::new();
    let mut build = build;
    let work = cache_dir.join("dbsnp").join("tmp");
    std::fs::create_dir_all(&work)?;
    let (mut runs_rs, mut runs_pos) = (Vec::new(), Vec::new());
    let mut buf: Vec<DbRec> = Vec::with_capacity(chunk.min(1 << 24));
    let (mut kept, mut skipped) = (0u64, 0u64);
    let flush = |buf: &mut Vec<DbRec>, runs_rs: &mut Vec<PathBuf>, runs_pos: &mut Vec<PathBuf>| -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let n = runs_rs.len();
        let (a, b) = (work.join(format!("rs-{n}.run")), work.join(format!("pos-{n}.run")));
        write_run(buf, false, &a)?;
        write_run(buf, true, &b)?;
        runs_rs.push(a);
        runs_pos.push(b);
        buf.clear();
        Ok(())
    };
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.starts_with('#') {
            if line.starts_with("##") {
                header.push_str(&line);
            } else if build.is_none() {
                let l = header.to_ascii_lowercase();
                build = if l.contains("grch38") || l.contains("hg38") {
                    Some(Build::GRCh38)
                } else if l.contains("grch37") || l.contains("hg19") {
                    Some(Build::GRCh37)
                } else {
                    None
                };
                if build.is_none() {
                    return Err(AppError::invalid("cannot tell the dbSNP VCF's build from its header; pass --build"));
                }
            }
            continue;
        }
        let mut f = line.split('\t');
        let (Some(c), Some(p), Some(id), Some(r), Some(a)) = (f.next(), f.next(), f.next(), f.next(), f.next()) else {
            skipped += 1;
            continue;
        };
        let contig = primary_index(&normalize_chrom(c));
        let (Some(contig), Ok(pos)) = (contig, p.parse::<u32>()) else {
            skipped += 1;
            continue;
        };
        let alts = a.trim().split(',').map(base_code).filter(|c| *c > 0).fold(0u8, |m, c| m | (1 << (c - 1)));
        for rs in id.split(';').filter_map(rs_number) {
            buf.push(DbRec { rs, contig: contig as u8, pos, reference: base_code(r), alts });
            kept += 1;
        }
        if buf.len() >= chunk {
            flush(&mut buf, &mut runs_rs, &mut runs_pos)?;
            progress(&format!("indexed {kept} rsids"));
        }
    }
    flush(&mut buf, &mut runs_rs, &mut runs_pos)?;
    let build = build.ok_or_else(|| AppError::invalid("dbSNP VCF has no header; pass --build"))?;
    let (by_rs, by_pos) = dbsnp_paths(cache_dir, build);
    merge_runs(&runs_rs, false, &by_rs)?;
    merge_runs(&runs_pos, true, &by_pos)?;
    let _ = std::fs::remove_dir_all(&work);
    Ok((build, kept, skipped))
}

struct DbIndex {
    file: File,
    n: u64,
}

impl DbIndex {
    fn open(p: &Path) -> Option<Self> {
        let mut file = File::open(p).ok()?;
        let mut m = [0u8; 8];
        file.read_exact(&mut m).ok()?;
        (&m == DB_MAGIC).then_some(())?;
        let n = (file.metadata().ok()?.len() - 8) / DREC as u64;
        Some(Self { file, n })
    }
    fn get(&mut self, i: u64) -> Result<DbRec> {
        let mut b = [0u8; DREC];
        self.file.seek(SeekFrom::Start(8 + i * DREC as u64))?;
        self.file.read_exact(&mut b)?;
        Ok(DbRec::decode(&b))
    }
    fn find<K: Ord>(&mut self, key: K, f: impl Fn(&DbRec) -> K) -> Result<Option<DbRec>> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if f(&self.get(mid)?) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < self.n {
            let r = self.get(lo)?;
            if f(&r) == key {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
}

/// Resolves rsids to coordinates (and back) for a build.
pub struct Resolver {
    curated: HashMap<String, CuratedRow>,
    cache_dir: PathBuf,
    db: HashMap<(Build, bool), Option<DbIndex>>,
}

impl Resolver {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            curated: curated().into_iter().map(|r| (r.rsid.clone(), r)).collect(),
            cache_dir: cache_dir.to_path_buf(),
            db: HashMap::new(),
        }
    }

    fn index(&mut self, build: Build, by_pos: bool) -> Option<&mut DbIndex> {
        let cache = &self.cache_dir;
        self.db
            .entry((build, by_pos))
            .or_insert_with(|| {
                let (a, b) = dbsnp_paths(cache, build);
                DbIndex::open(if by_pos { &b } else { &a })
            })
            .as_mut()
    }

    pub fn has_dbsnp(&mut self, build: Build) -> bool {
        self.index(build, false).is_some()
    }

    pub fn curated_row(&self, rsid: &str) -> Option<&CuratedRow> {
        self.curated.get(rsid)
    }

    /// rsid -> coordinate in `build`.
    pub fn resolve(&mut self, rsid: &str, build: Build) -> Result<Option<Coord>> {
        if let Some(r) = self.curated.get(rsid) {
            let (pos, reference, alt) = match build {
                Build::GRCh37 => (r.grch37_pos, &r.grch37_ref, &r.grch37_alt),
                Build::GRCh38 => (r.grch38_pos, &r.grch38_ref, &r.grch38_alt),
                Build::Unknown => return Ok(None),
            };
            return Ok(Some(Coord {
                rsid: r.rsid.clone(),
                chrom: r.chrom.clone(),
                pos,
                reference: Some(reference.clone()),
                alt: alt.clone(),
                source: "curated",
            }));
        }
        let Some(rs) = rs_number(rsid) else { return Ok(None) };
        match self.index(build, false) {
            Some(ix) => Ok(ix.find(rs, |r| r.rs)?.map(|r| r.coord())),
            None => Ok(None),
        }
    }

    /// (chrom, pos) -> rsid/reference in `build`.
    pub fn at(&mut self, build: Build, chrom: &str, pos: u32) -> Result<Option<Coord>> {
        let hit = self.curated.values().find(|r| {
            r.chrom == chrom
                && match build {
                    Build::GRCh37 => r.grch37_pos == pos,
                    Build::GRCh38 => r.grch38_pos == pos,
                    Build::Unknown => false,
                }
        });
        if let Some(r) = hit.map(|r| r.rsid.clone()) {
            return self.resolve(&r, build);
        }
        let Some(contig) = primary_index(chrom) else { return Ok(None) };
        match self.index(build, true) {
            Some(ix) => Ok(ix.find((contig as u8, pos), |r| (r.contig, r.pos))?.map(|r| r.coord())),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_rows_parse() {
        let rows = curated();
        assert!(rows.len() >= 8);
        let apoe = rows.iter().find(|r| r.rsid == "rs429358").unwrap();
        assert_eq!((apoe.grch37_pos, apoe.grch38_pos), (45_411_941, 44_908_684));
        assert!(rows
            .iter()
            .all(|r| r.source.contains("ncbi.nlm.nih.gov/snp/") && r.grch37_pos > 0 && r.grch38_pos > 0));
    }

    #[test]
    fn dbsnp_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let vcf = dir.path().join("dbsnp.vcf");
        std::fs::write(
            &vcf,
            "##fileformat=VCFv4.2\n##reference=GRCh38.p14\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n\
             NC_000001.11\t100\trs30\tA\tG\t.\t.\t.\nNC_000001.11\t200\trs10\tC\tT,G\t.\t.\t.\n\
             NC_000002.12\t50\trs20\tAT\tA\t.\t.\t.\nNT_187361.1\t5\trs40\tA\tC\t.\t.\t.\n",
        )
        .unwrap();
        let (b, kept, skipped) = import_dbsnp(&vcf, dir.path(), None, 2, &|_| {}).unwrap();
        assert_eq!((b, kept, skipped), (Build::GRCh38, 3, 1));
        let mut r = Resolver::new(dir.path());
        let c = r.resolve("rs10", Build::GRCh38).unwrap().unwrap();
        assert_eq!((c.chrom.as_str(), c.pos, c.reference.as_deref()), ("1", 200, Some("C")));
        assert_eq!(c.alt, vec!["G".to_string(), "T".to_string()]);
        assert_eq!(r.resolve("rs20", Build::GRCh38).unwrap().unwrap().reference, None);
        assert!(r.resolve("rs99", Build::GRCh38).unwrap().is_none());
        assert_eq!(r.at(Build::GRCh38, "1", 100).unwrap().unwrap().rsid, "rs30");
        assert_eq!(r.at(Build::GRCh38, "19", 44_908_684).unwrap().unwrap().rsid, "rs429358");
        assert!(r.resolve("rs10", Build::GRCh37).unwrap().is_none());
    }
}
