//! Streaming parsers for genotype sources: array exports and VCF/gVCF.

pub mod array;
pub mod vcf;

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::error::{AppError, Result};
use crate::model::{Build, Call};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InputFormat {
    Auto,
    #[value(name = "23andme")]
    TwentyThreeAndMe,
    Ancestry,
    Myheritage,
    Ftdna,
    Vcf,
}

impl InputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        <Self as clap::ValueEnum>::from_str(s, true).ok()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::TwentyThreeAndMe => "23andme",
            Self::Ancestry => "ancestry",
            Self::Myheritage => "myheritage",
            Self::Ftdna => "ftdna",
            Self::Vcf => "vcf",
        }
    }
}

/// What a parser learned about the source besides the calls themselves.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// 23andme | ancestry | myheritage | ftdna | vcf | gvcf
    pub source_format: String,
    pub assay: &'static str,
    pub build: Build,
    /// header | contig-lengths | assumed
    pub build_evidence: &'static str,
    pub sample: Option<String>,
    pub chip: Option<String>,
    pub warnings: Vec<String>,
    /// VCF contig header lines (name, length) in file order.
    pub contigs: Vec<(String, Option<u64>)>,
}

/// Open a possibly gzip/bgzip-compressed text file.
pub fn open_text(path: &Path) -> Result<Box<dyn BufRead>> {
    let mut f = File::open(path).map_err(|e| AppError::io(format!("opening {}: {e}", path.display())))?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic)?;
    drop(f);
    let f = File::open(path)?;
    if n == 2 && magic == [0x1f, 0x8b] {
        Ok(Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::MultiGzDecoder::new(BufReader::with_capacity(1 << 20, f)),
        )))
    } else if n == 2 && magic == *b"PK" {
        Err(AppError::invalid(format!(
            "{} is a zip archive; unzip it first and import the .txt/.csv inside",
            path.display()
        )))
    } else {
        Ok(Box::new(BufReader::with_capacity(1 << 20, f)))
    }
}

/// Guess the input format from the first lines of the file.
pub fn detect(path: &Path) -> Result<InputFormat> {
    let mut r = open_text(path)?;
    let mut head = Vec::new();
    let mut line = String::new();
    while head.len() < 60 {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        head.push(line.trim_end().to_string());
    }
    let text = head.join("\n");
    let lower = text.to_ascii_lowercase();
    let first_data = head.iter().find(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| l.to_ascii_lowercase());
    let fmt = if head.first().is_some_and(|l| l.starts_with("##fileformat=VCF")) {
        InputFormat::Vcf
    } else if lower.contains("23andme") {
        InputFormat::TwentyThreeAndMe
    } else if lower.contains("ancestrydna")
        || first_data.as_deref().is_some_and(|l| l.starts_with("rsid\tchromosome\tposition\tallele1"))
    {
        InputFormat::Ancestry
    } else if lower.contains("myheritage") {
        InputFormat::Myheritage
    } else if first_data.as_deref().is_some_and(|l| l.replace('"', "").starts_with("rsid,chromosome,position,result")) {
        InputFormat::Ftdna
    } else if lower.contains("# rsid\tchromosome\tposition\tgenotype")
        || first_data
            .as_deref()
            .is_some_and(|l| l.split('\t').count() == 4 && (l.starts_with("rs") || l.starts_with('i')))
    {
        InputFormat::TwentyThreeAndMe
    } else {
        return Err(AppError::invalid(format!(
            "cannot detect the format of {}; pass --format 23andme|ancestry|myheritage|ftdna|vcf",
            path.display()
        )));
    };
    Ok(fmt)
}

/// Parse `path` in `format`, streaming every call into `sink`.
pub fn parse(
    path: &Path,
    format: InputFormat,
    sample: Option<&str>,
    sink: &mut dyn FnMut(Call) -> Result<()>,
) -> Result<SourceInfo> {
    let format = if format == InputFormat::Auto { detect(path)? } else { format };
    let reader = open_text(path)?;
    match format {
        InputFormat::Vcf => vcf::parse(reader, sample, sink),
        f => array::parse(reader, f, sink),
    }
}
