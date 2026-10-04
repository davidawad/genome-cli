//! Native UCSC chain-file liftover (GRCh37 <-> GRCh38).
//!
//! Chain format (https://genome.ucsc.edu/goldenPath/help/chain.html):
//! `chain score tName tSize tStrand tStart tEnd qName qSize qStrand qStart qEnd id`
//! followed by `size dt dq` lines and a final `size` line. "t" is the source
//! assembly, "q" the destination. Coordinates are 0-based half-open; on a `-`
//! qStrand they count from the end of the reverse-complemented query.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};
use crate::model::{normalize_chrom, Build};

#[derive(Debug, Clone)]
struct Block {
    t_start: u64,
    t_end: u64,
    q_chrom: u32,
    q_start: u64,
    q_size: u64,
    q_neg: bool,
    score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lifted {
    pub chrom: String,
    pub pos: u32,
    /// Destination is on the opposite strand: alleles must be complemented.
    pub reverse: bool,
}

#[derive(Debug, Default)]
pub struct Chain {
    /// Per source contig: blocks sorted by t_start and the running max of t_end.
    blocks: HashMap<String, (Vec<Block>, Vec<u64>)>,
    q_names: Vec<String>,
}

impl Chain {
    pub fn parse(reader: impl BufRead) -> Result<Self> {
        let mut raw: HashMap<String, Vec<Block>> = HashMap::new();
        let mut q_names: Vec<String> = Vec::new();
        let mut q_ids: HashMap<String, u32> = HashMap::new();
        // Current chain header state.
        let mut cur: Option<(String, u64, u32, u64, u64, bool, f64)> = None; // tName, t, qId, q, qSize, neg, score
        for (n, line) in reader.lines().enumerate() {
            let line = line?;
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.is_empty() || f[0].starts_with('#') {
                continue;
            }
            let bad = || AppError::invalid(format!("chain line {}: malformed: {line}", n + 1));
            let num = |s: &str| s.parse::<u64>().map_err(|_| bad());
            if f[0] == "chain" {
                if f.len() < 12 {
                    return Err(bad());
                }
                let q = normalize_chrom(f[7]);
                let qid = *q_ids.entry(q.clone()).or_insert_with(|| {
                    q_names.push(q.clone());
                    (q_names.len() - 1) as u32
                });
                if f[4] != "+" {
                    return Err(AppError::invalid(format!("chain line {}: tStrand must be +", n + 1)));
                }
                cur = Some((
                    normalize_chrom(f[2]),
                    num(f[5])?,
                    qid,
                    num(f[10])?,
                    num(f[8])?,
                    f[9] == "-",
                    f[1].parse::<f64>().map_err(|_| bad())?,
                ));
                continue;
            }
            let Some((t_name, t, qid, q, q_size, neg, score)) = cur.as_mut() else { return Err(bad()) };
            let size = num(f[0])?;
            raw.entry(t_name.clone()).or_default().push(Block {
                t_start: *t,
                t_end: *t + size,
                q_chrom: *qid,
                q_start: *q,
                q_size: *q_size,
                q_neg: *neg,
                score: *score,
            });
            if f.len() >= 3 {
                *t += size + num(f[1])?;
                *q += size + num(f[2])?;
            } else {
                cur = None;
            }
        }
        let blocks = raw
            .into_iter()
            .map(|(k, mut v)| {
                v.sort_by_key(|b| b.t_start);
                let mut m = 0;
                let max_end = v
                    .iter()
                    .map(|b| {
                        m = m.max(b.t_end);
                        m
                    })
                    .collect();
                (k, (v, max_end))
            })
            .collect();
        Ok(Self { blocks, q_names })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let r = crate::parse::open_text(path)?;
        Self::parse(r).map_err(|e| e.context(path.display()))
    }

    /// Map a 1-based position. Returns hits ordered by chain score (best first).
    pub fn lift(&self, chrom: &str, pos: u32) -> Vec<Lifted> {
        let Some((blocks, max_end)) = self.blocks.get(chrom) else { return Vec::new() };
        let x = u64::from(pos).saturating_sub(1);
        let mut i = blocks.partition_point(|b| b.t_start <= x);
        let mut hits: Vec<(f64, Lifted)> = Vec::new();
        while i > 0 {
            i -= 1;
            if max_end[i] <= x {
                break;
            }
            let b = &blocks[i];
            if x < b.t_end {
                let q = b.q_start + (x - b.t_start);
                let q0 = if b.q_neg { b.q_size - 1 - q } else { q };
                let lifted =
                    Lifted { chrom: self.q_names[b.q_chrom as usize].clone(), pos: (q0 + 1) as u32, reverse: b.q_neg };
                hits.push((b.score, lifted));
            }
        }
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.into_iter().map(|(_, l)| l).collect()
    }
}

/// A pinned UCSC chain file.
pub struct ChainSpec {
    pub from: Build,
    pub to: Build,
    pub file: &'static str,
    pub url_path: &'static str,
    pub sha256: &'static str,
}

/// sha256 of the UCSC files as downloaded 2026-10-03 (UCSC md5sum.txt:
/// hg19ToHg38 35887f73fe5e2231656504d1f6430900, hg38ToHg19 ff3031d93792f4cbb86af44055efd903).
pub const CHAINS: &[ChainSpec] = &[
    ChainSpec {
        from: Build::GRCh37,
        to: Build::GRCh38,
        file: "hg19ToHg38.over.chain.gz",
        url_path: "hg19/liftOver/hg19ToHg38.over.chain.gz",
        sha256: "5c0598e500ceb5a78c73086929e8ef993aec309bcafb595139b53d440b125a1d",
    },
    ChainSpec {
        from: Build::GRCh38,
        to: Build::GRCh37,
        file: "hg38ToHg19.over.chain.gz",
        url_path: "hg38/liftOver/hg38ToHg19.over.chain.gz",
        sha256: "14a712e8e147d9fc8e9d87d51977b46f6f8ddb93efbe5d0843d86b6205f587b1",
    },
];

pub fn spec(from: Build, to: Build) -> Result<&'static ChainSpec> {
    CHAINS
        .iter()
        .find(|c| c.from == from && c.to == to)
        .ok_or_else(|| AppError::usage(format!("no liftover chain from {} to {}", from.as_str(), to.as_str())))
}

pub fn chain_path(cache_dir: &Path, s: &ChainSpec) -> PathBuf {
    cache_dir.join("chains").join(s.file)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(format!("{:x}", h.finalize()))
}

/// Ensure the chain file is cached (downloading + verifying it if needed) and return its path.
pub fn ensure_chain(
    cache_dir: &Path,
    base_url: &str,
    s: &ChainSpec,
    offline: bool,
    info: &dyn Fn(&str),
) -> Result<PathBuf> {
    let p = chain_path(cache_dir, s);
    if p.exists() {
        return Ok(p);
    }
    if offline {
        return Err(AppError::not_found(format!(
            "chain file {} is not cached and offline mode is on (run `genome liftover --fetch` online)",
            p.display()
        )));
    }
    let url = format!("{}/{}", base_url.trim_end_matches('/'), s.url_path);
    info(&format!("downloading {url}"));
    crate::fetch::download(&url, &p, Some(crate::fetch::Checksum::Sha256(s.sha256)))?;
    Ok(p)
}

/// Loads chains lazily and caches them for the process lifetime.
pub struct Lifter {
    pub cache_dir: PathBuf,
    pub base_url: String,
    pub offline: bool,
    pub info: fn(&str),
    loaded: HashMap<(Build, Build), Chain>,
}

impl Lifter {
    pub fn new(cache_dir: PathBuf, base_url: String, offline: bool, info: fn(&str)) -> Self {
        Self { cache_dir, base_url, offline, info, loaded: HashMap::new() }
    }

    pub fn chain(&mut self, from: Build, to: Build) -> Result<&Chain> {
        if !self.loaded.contains_key(&(from, to)) {
            let s = spec(from, to)?;
            let p = ensure_chain(&self.cache_dir, &self.base_url, s, self.offline, &self.info)?;
            self.loaded.insert((from, to), Chain::load(&p)?);
        }
        Ok(&self.loaded[&(from, to)])
    }

    /// Use an explicit chain file for `from` -> `to` instead of the cached UCSC one.
    pub fn set_chain(&mut self, from: Build, to: Build, chain: Chain) {
        self.loaded.insert((from, to), chain);
    }

    /// Best hit for a position, or None when unmapped.
    pub fn lift(&mut self, from: Build, to: Build, chrom: &str, pos: u32) -> Result<Option<Lifted>> {
        if from == to {
            return Ok(Some(Lifted { chrom: chrom.to_string(), pos, reverse: false }));
        }
        if from == Build::Unknown || to == Build::Unknown {
            return Err(AppError::invalid("cannot lift over to/from an unknown build"));
        }
        Ok(self.chain(from, to)?.lift(chrom, pos).into_iter().next())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Synthetic chain: chr1 [0,100) -> chr1 [1000,1100) with a 10-base gap in
    /// the source at 50; chr2 maps to the reverse strand of chr7 (size 1000).
    pub const SYNTH: &str = "chain 1000 chr1 5000 + 0 110 chr1 6000 + 1000 1105 1\n50 10 5\n50\n\n\
chain 500 chr2 3000 + 100 200 chr7 1000 - 0 100 2\n100\n";

    #[test]
    fn lifts_forward_and_reverse() {
        let c = Chain::parse(SYNTH.as_bytes()).unwrap();
        // pos 1 (0-based 0) -> 1001
        assert_eq!(c.lift("1", 1), vec![Lifted { chrom: "1".into(), pos: 1001, reverse: false }]);
        assert_eq!(c.lift("1", 50)[0].pos, 1050);
        // 0-based 50..60 is a source gap.
        assert!(c.lift("1", 55).is_empty());
        // 0-based 60 -> q = 1000 + 50 + 5 = 1055 -> 1-based 1056
        assert_eq!(c.lift("1", 61)[0].pos, 1056);
        assert!(c.lift("1", 200).is_empty());
        // chr2 0-based 100 -> q 0 on minus strand -> + coordinate 999 -> 1-based 1000
        let r = &c.lift("chr2".trim_start_matches("chr"), 101)[0];
        assert_eq!((r.chrom.as_str(), r.pos, r.reverse), ("7", 1000, true));
        assert_eq!(c.lift("2", 200)[0].pos, 901);
        assert!(c.lift("3", 1).is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Chain::parse("50 10 5\n".as_bytes()).is_err());
        assert!(Chain::parse("chain x\n".as_bytes()).is_err());
    }
}
