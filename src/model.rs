//! The shared genotype model: builds, contig naming, zygosity, call records.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Build {
    GRCh37,
    GRCh38,
    #[serde(rename = "unknown")]
    Unknown,
}

impl Build {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GRCh37 => "GRCh37",
            Self::GRCh38 => "GRCh38",
            Self::Unknown => "unknown",
        }
    }
    /// Accepts GRCh37/hg19/b37/37 and GRCh38/hg38/38 (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "grch37" | "hg19" | "b37" | "37" | "hs37d5" => Some(Self::GRCh37),
            "grch38" | "hg38" | "b38" | "38" => Some(Self::GRCh38),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::GRCh37 => Self::GRCh38,
            Self::GRCh38 => Self::GRCh37,
            Self::Unknown => Self::Unknown,
        }
    }
    /// Pseudo-autosomal regions of X (1-based inclusive), excluded from X heterozygosity.
    pub fn par_regions(self) -> &'static [(u32, u32)] {
        match self {
            Self::GRCh37 => &[(60_001, 2_699_520), (154_931_044, 155_260_560)],
            Self::GRCh38 => &[(10_001, 2_781_479), (155_701_383, 156_030_895)],
            Self::Unknown => &[],
        }
    }
    pub fn in_par(self, chrom: &str, pos: u32) -> bool {
        chrom == "X" && self.par_regions().iter().any(|(s, e)| (*s..=*e).contains(&pos))
    }
    /// Build from the length of chromosome 1.
    pub fn from_chr1_length(len: u64) -> Option<Self> {
        match len {
            249_250_621 => Some(Self::GRCh37),
            248_956_422 => Some(Self::GRCh38),
            _ => None,
        }
    }
}

/// Normalise a contig name: strip `chr`, map array chromosome codes
/// (23=X, 24=Y, 25=X PAR, 26=MT, XY=X) and M -> MT.
pub fn normalize_chrom(raw: &str) -> String {
    let s = raw.trim();
    let base = s.strip_prefix("chr").or_else(|| s.strip_prefix("Chr")).or_else(|| s.strip_prefix("CHR")).unwrap_or(s);
    match base {
        "23" | "25" | "XY" | "x" => "X".into(),
        "24" | "y" => "Y".into(),
        "26" | "M" | "m" | "MT" | "mt" | "Mt" => "MT".into(),
        b => {
            // RefSeq accessions as used by dbSNP (NC_000001.11 -> 1).
            if let Some(n) =
                b.strip_prefix("NC_0000").and_then(|r| r.split('.').next()).and_then(|n| n.parse::<u8>().ok())
            {
                return match n {
                    1..=22 => n.to_string(),
                    23 => "X".into(),
                    24 => "Y".into(),
                    _ => b.to_string(),
                };
            }
            if b == "NC_012920" || b.starts_with("NC_012920.") {
                return "MT".into();
            }
            b.to_string()
        }
    }
}

/// Sort/index position of a primary chromosome (1..=22, X=23, Y=24, MT=25).
pub fn primary_index(norm: &str) -> Option<u16> {
    match norm {
        "X" => Some(23),
        "Y" => Some(24),
        "MT" => Some(25),
        n => n.parse::<u16>().ok().filter(|v| (1..=22).contains(v)),
    }
}

pub fn is_primary(norm: &str) -> bool {
    primary_index(norm).is_some()
}

/// Keys of `by_chrom` in summaries, in display order.
pub fn by_chrom_keys() -> Vec<String> {
    (1..=22).map(|i| i.to_string()).chain(["X", "Y", "MT", "other_contigs"].map(String::from)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Zygosity {
    Het,
    HomRef,
    HomAlt,
    /// Homozygous, but the reference allele is unknown (array site without a coordinate-table entry).
    Hom,
    Hemi,
    NoCall,
}

impl Zygosity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Het => "het",
            Self::HomRef => "hom_ref",
            Self::HomAlt => "hom_alt",
            Self::Hom => "hom",
            Self::Hemi => "hemi",
            Self::NoCall => "no_call",
        }
    }
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(c: u8) -> Self {
        match c {
            0 => Self::Het,
            1 => Self::HomRef,
            2 => Self::HomAlt,
            3 => Self::Hom,
            4 => Self::Hemi,
            _ => Self::NoCall,
        }
    }
}

/// Classify array genotype letters (`AG`, `AA`, `A`, `--`, `DI`, ...).
pub fn array_zygosity(genotype: &str, reference: Option<&str>) -> Zygosity {
    let g: Vec<char> = genotype.chars().collect();
    match g.as_slice() {
        [] => Zygosity::NoCall,
        _ if g.iter().any(|c| matches!(c, '-' | '0' | '?' | 'N' | ' ')) => Zygosity::NoCall,
        [_] => Zygosity::Hemi,
        [a, b] if a != b => Zygosity::Het,
        [a, _] => match reference {
            Some(r) if r.len() == 1 && r.starts_with(*a) => Zygosity::HomRef,
            Some(r) if r.len() == 1 => Zygosity::HomAlt,
            _ => Zygosity::Hom,
        },
        _ => Zygosity::NoCall,
    }
}

/// One genotype call as stored for a kit (coordinates in the kit's build).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Call {
    pub chrom: String,
    pub pos: u32,
    /// Last reference base covered (== pos except for gVCF reference blocks and multi-base REF).
    pub end: u32,
    pub rsid: Option<String>,
    pub reference: Option<String>,
    pub alt: Vec<String>,
    pub genotype: String,
    pub gt: Option<String>,
    pub zygosity: Option<Zygosity>,
    pub filter: Option<String>,
    pub quality: Option<f32>,
    pub depth: Option<u32>,
    /// gVCF reference block (`<NON_REF>` / `<*>` with END).
    pub ref_block: bool,
}

impl Call {
    pub fn zyg(&self) -> Zygosity {
        self.zygosity.unwrap_or(Zygosity::NoCall)
    }
    pub fn is_called(&self) -> bool {
        self.zyg() != Zygosity::NoCall
    }
}

/// Numeric part of an `rs` identifier.
pub fn rs_number(id: &str) -> Option<u32> {
    id.strip_prefix("rs").or_else(|| id.strip_prefix("RS")).and_then(|n| n.parse().ok())
}

pub fn complement(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A' => 'T',
            'T' => 'A',
            'C' => 'G',
            'G' => 'C',
            'a' => 't',
            't' => 'a',
            'c' => 'g',
            'g' => 'c',
            o => o,
        })
        .collect()
}

/// Unordered allele multiset of a genotype for concordance (hemizygous `A` == `AA`).
pub fn allele_key(genotype: &str) -> Option<Vec<String>> {
    let mut alleles: Vec<String> = if genotype.contains('/') {
        genotype.split('/').map(str::to_string).collect()
    } else {
        genotype.chars().map(|c| c.to_string()).collect()
    };
    if alleles.is_empty() || alleles.iter().any(|a| a.is_empty() || a == "-" || a == "." || a == "0" || a == "N") {
        return None;
    }
    if alleles.len() == 1 {
        alleles.push(alleles[0].clone());
    }
    alleles.sort();
    Some(alleles)
}

/// Parse `CHR:POS` (pos may use `,`/`_` separators).
pub fn parse_locus(s: &str) -> Option<(String, u32)> {
    let (c, p) = s.rsplit_once(':')?;
    let p: u32 = p.replace([',', '_'], "").parse().ok()?;
    (p > 0 && !c.is_empty()).then(|| (normalize_chrom(c), p))
}

/// A region `CHR`, `CHR:START-END`; START/END accept `k`/`M` suffixes (`chr19:44.9M-45.0M`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub raw_chrom: String,
    pub chrom: String,
    pub start: u32,
    pub end: u32,
}

fn parse_scaled(s: &str) -> Option<u32> {
    let s = s.replace([',', '_'], "");
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1e3),
        'm' | 'M' => (&s[..s.len() - 1], 1e6),
        _ => (s.as_str(), 1.0),
    };
    let v: f64 = num.parse().ok()?;
    let r = (v * mult).round();
    (r >= 0.0 && r <= u32::MAX as f64).then_some(r as u32)
}

impl Region {
    pub fn parse(s: &str) -> Option<Self> {
        let (c, range) = match s.split_once(':') {
            Some((c, r)) => (c, Some(r)),
            None => (s, None),
        };
        let (start, end) = match range {
            None => (1, u32::MAX),
            Some(r) => match r.split_once('-') {
                Some((a, b)) => (parse_scaled(a)?.max(1), parse_scaled(b)?),
                None => {
                    let p = parse_scaled(r)?;
                    (p, p)
                }
            },
        };
        (!c.is_empty() && start <= end).then(|| Self {
            raw_chrom: c.to_string(),
            chrom: normalize_chrom(c),
            start,
            end,
        })
    }
    pub fn contains(&self, chrom: &str, pos: u32) -> bool {
        chrom == self.chrom && (self.start..=self.end).contains(&pos)
    }
    /// samtools/bcftools region string using the given contig name.
    pub fn to_tool_string(&self, contig: &str) -> String {
        if self.start <= 1 && self.end == u32::MAX {
            contig.to_string()
        } else {
            format!("{contig}:{}-{}", self.start, self.end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrom_names() {
        assert_eq!(normalize_chrom("chr1"), "1");
        assert_eq!(normalize_chrom("chrM"), "MT");
        assert_eq!(normalize_chrom("23"), "X");
        assert_eq!(normalize_chrom("25"), "X");
        assert_eq!(normalize_chrom("26"), "MT");
        assert_eq!(normalize_chrom("NC_000019.10"), "19");
        assert_eq!(normalize_chrom("NC_000023.11"), "X");
        assert_eq!(normalize_chrom("chr1_KI270706v1_random"), "1_KI270706v1_random");
        assert!(!is_primary(&normalize_chrom("chrUn_KI270302v1")));
        assert!(!is_primary(&normalize_chrom("HLA-A*01:01:01:01")));
        assert!(!is_primary(&normalize_chrom("chrEBV")));
        assert!(is_primary("22"));
    }

    #[test]
    fn par_boundaries() {
        assert!(Build::GRCh37.in_par("X", 60_001));
        assert!(!Build::GRCh37.in_par("X", 60_000));
        assert!(Build::GRCh38.in_par("X", 156_030_895));
        assert!(!Build::GRCh38.in_par("X", 2_781_480));
        assert!(!Build::GRCh38.in_par("7", 20_000));
    }

    #[test]
    fn array_zygosity_cases() {
        assert_eq!(array_zygosity("AG", None), Zygosity::Het);
        assert_eq!(array_zygosity("AA", None), Zygosity::Hom);
        assert_eq!(array_zygosity("AA", Some("A")), Zygosity::HomRef);
        assert_eq!(array_zygosity("GG", Some("A")), Zygosity::HomAlt);
        assert_eq!(array_zygosity("A", None), Zygosity::Hemi);
        assert_eq!(array_zygosity("--", None), Zygosity::NoCall);
        assert_eq!(array_zygosity("00", None), Zygosity::NoCall);
        assert_eq!(array_zygosity("DI", None), Zygosity::Het);
    }

    #[test]
    fn regions_and_loci() {
        let r = Region::parse("chr19:44.9M-45.0M").unwrap();
        assert_eq!((r.chrom.as_str(), r.start, r.end), ("19", 44_900_000, 45_000_000));
        assert_eq!(r.to_tool_string("chr19"), "chr19:44900000-45000000");
        assert_eq!(Region::parse("X").unwrap().end, u32::MAX);
        assert_eq!(parse_locus("chr19:44,908,684"), Some(("19".into(), 44_908_684)));
        assert!(parse_locus("19").is_none());
    }

    #[test]
    fn allele_keys() {
        assert_eq!(allele_key("CT"), allele_key("TC"));
        assert_eq!(allele_key("A"), allele_key("AA"));
        assert_eq!(allele_key("--"), None);
        assert_eq!(allele_key("AT/A"), Some(vec!["A".into(), "AT".into()]));
    }
}
