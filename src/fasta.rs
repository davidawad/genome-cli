//! Random access to reference bases through a samtools `.fai` index (uncompressed FASTA).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{AppError, Result};
use crate::model::normalize_chrom;

struct FaiEntry {
    len: u64,
    offset: u64,
    line_bases: u64,
    line_bytes: u64,
}

pub struct Fasta {
    file: File,
    index: HashMap<String, FaiEntry>,
}

impl Fasta {
    pub fn open(path: &Path) -> Result<Self> {
        let fai = PathBuf::from(format!("{}.fai", path.display()));
        let text = std::fs::read_to_string(&fai)
            .map_err(|e| AppError::not_found(format!("{}: {e} (run `samtools faidx`)", fai.display())))?;
        let index = text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
                Some((
                    normalize_chrom(f.first()?),
                    FaiEntry { len: n(1)?, offset: n(2)?, line_bases: n(3)?, line_bytes: n(4)? },
                ))
            })
            .collect();
        let file = File::open(path).map_err(|e| AppError::io(format!("{}: {e}", path.display())))?;
        Ok(Self { file, index })
    }

    /// Upper-case bases [pos, pos+len) (1-based), or None outside the contig.
    pub fn bases(&mut self, chrom: &str, pos: u32, len: u32) -> Result<Option<String>> {
        let Some(e) = self.index.get(chrom) else { return Ok(None) };
        let start = u64::from(pos).saturating_sub(1);
        if pos == 0 || start + u64::from(len) > e.len || e.line_bases == 0 {
            return Ok(None);
        }
        let mut out = String::with_capacity(len as usize);
        for i in start..start + u64::from(len) {
            let off = e.offset + (i / e.line_bases) * e.line_bytes + i % e.line_bases;
            let mut b = [0u8; 1];
            self.file.seek(SeekFrom::Start(off))?;
            self.file.read_exact(&mut b)?;
            out.push((b[0] as char).to_ascii_uppercase());
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bases() {
        let d = tempfile::tempdir().unwrap();
        let fa = d.path().join("r.fa");
        std::fs::write(&fa, ">chr1 desc\nACGTA\ncgtac\nGG\n").unwrap();
        std::fs::write(d.path().join("r.fa.fai"), "chr1\t12\t11\t5\t6\n").unwrap();
        let mut f = Fasta::open(&fa).unwrap();
        assert_eq!(f.bases("1", 1, 1).unwrap().as_deref(), Some("A"));
        assert_eq!(f.bases("1", 5, 3).unwrap().as_deref(), Some("ACG"));
        assert_eq!(f.bases("1", 12, 1).unwrap().as_deref(), Some("G"));
        assert_eq!(f.bases("1", 13, 1).unwrap(), None);
        assert_eq!(f.bases("2", 1, 1).unwrap(), None);
    }
}
