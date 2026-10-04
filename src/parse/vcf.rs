//! Streaming VCF / gVCF reader (plain or bgzip-compressed).
//!
//! WGS VCFs usually list variant sites only: a covered site that is absent
//! is homozygous reference. gVCFs add explicit reference blocks
//! (`ALT=<NON_REF>` or `<*>` with `INFO/END`).

use std::io::BufRead;

use super::SourceInfo;
use crate::error::{AppError, Result};
use crate::model::{normalize_chrom, Build, Call, Zygosity};

fn header_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let inner = line.split_once('<')?.1.trim_end_matches('>');
    inner.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

fn build_from_text(text: &str) -> Option<Build> {
    let l = text.to_ascii_lowercase();
    if ["grch38", "hg38", "gca_000001405.15", "b38", "hs38"].iter().any(|k| l.contains(k)) {
        Some(Build::GRCh38)
    } else if ["grch37", "hg19", "b37", "hs37d5", "human_g1k_v37", "gca_000001405.1"].iter().any(|k| l.contains(k)) {
        Some(Build::GRCh37)
    } else {
        None
    }
}

fn is_symbolic(a: &str) -> bool {
    a.starts_with('<') || a == "*" || a.contains('[') || a.contains(']')
}

/// Convert a GT string into (genotype letters, zygosity).
pub fn genotype_letters(gt: &str, reference: &str, alts: &[&str]) -> (String, Zygosity) {
    let idx: Vec<&str> = gt.split(['/', '|']).collect();
    if idx.iter().any(|i| *i == "." || i.is_empty()) {
        return (if idx.len() == 1 { "-".into() } else { "--".into() }, Zygosity::NoCall);
    }
    let alleles: Vec<&str> = idx
        .iter()
        .map(|i| match i.parse::<usize>() {
            Ok(0) => reference,
            Ok(k) => alts.get(k - 1).map_or("N", |a| if is_symbolic(a) { "N" } else { a }),
            Err(_) => "N",
        })
        .collect();
    let letters = if alleles.iter().all(|a| a.len() == 1) { alleles.concat() } else { alleles.join("/") };
    let zyg = match idx.as_slice() {
        [_] => Zygosity::Hemi,
        [a, rest @ ..] if rest.iter().all(|b| b == a) => {
            if *a == "0" {
                Zygosity::HomRef
            } else {
                Zygosity::HomAlt
            }
        }
        _ => Zygosity::Het,
    };
    (letters, zyg)
}

struct Header {
    sample_col: usize,
    sample: String,
    build: Build,
    evidence: &'static str,
    gvcf: bool,
    contigs: Vec<(String, Option<u64>)>,
    n_samples: usize,
}

fn parse_header(lines: &[String], want: Option<&str>) -> Result<Header> {
    let mut contigs = Vec::new();
    let mut text_build = None;
    let mut gvcf = false;
    for l in lines {
        if l.starts_with("##contig=") {
            if let Some(id) = header_value(l, "ID") {
                contigs.push((id.to_string(), header_value(l, "length").and_then(|v| v.parse().ok())));
            }
            if let Some(a) = header_value(l, "assembly") {
                text_build = text_build.or(build_from_text(a));
            }
        } else if l.starts_with("##reference=") || l.starts_with("##assembly=") || l.starts_with("##DRAGENCommandLine")
        {
            text_build = text_build.or(build_from_text(l));
        } else if l.starts_with("##GVCFBlock") || l.starts_with("##ALT=<ID=NON_REF") {
            gvcf = true;
        }
    }
    let length_build = contigs
        .iter()
        .find(|(id, _)| normalize_chrom(id) == "1")
        .and_then(|(_, len)| len.and_then(Build::from_chr1_length));
    let (build, evidence) = match (length_build, text_build) {
        (Some(b), _) => (b, "contig-lengths"),
        (None, Some(b)) => (b, "header"),
        (None, None) => (Build::Unknown, "assumed"),
    };
    let chrom_line = lines
        .iter()
        .find(|l| l.starts_with("#CHROM"))
        .ok_or_else(|| AppError::invalid("VCF has no #CHROM header line"))?;
    let cols: Vec<&str> = chrom_line.split('\t').collect();
    if cols.len() < 10 {
        return Err(AppError::invalid("VCF has no sample columns (sites-only VCF); nothing to import"));
    }
    let sample_col =
        match want {
            Some(w) => cols.iter().position(|c| *c == w).filter(|i| *i >= 9).ok_or_else(|| {
                AppError::not_found(format!("sample '{w}' not in VCF (have: {})", cols[9..].join(", ")))
            })?,
            None => 9,
        };
    Ok(Header {
        sample_col,
        sample: cols[sample_col].to_string(),
        build,
        evidence,
        gvcf,
        contigs,
        n_samples: cols.len() - 9,
    })
}

pub fn parse(
    mut reader: Box<dyn BufRead>,
    sample: Option<&str>,
    sink: &mut dyn FnMut(Call) -> Result<()>,
) -> Result<SourceInfo> {
    let mut header_lines = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let t = line.trim_end_matches(['\r', '\n']);
        if !t.starts_with('#') {
            break;
        }
        let done = t.starts_with("#CHROM");
        header_lines.push(t.to_string());
        if done {
            line.clear();
            break;
        }
    }
    let h = parse_header(&header_lines, sample)?;
    let mut warnings = Vec::new();
    if h.n_samples > 1 {
        warnings.push(format!("multi-sample VCF ({} samples); imported sample '{}'", h.n_samples, h.sample));
    }
    let mut ref_blocks = 0u64;
    let mut records = 0u64;
    let mut malformed = 0u64;
    let mut first = true;
    loop {
        if !first || line.is_empty() {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
        }
        first = false;
        let t = line.trim_end_matches(['\r', '\n']);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = t.split('\t').collect();
        if f.len() <= h.sample_col {
            malformed += 1;
            continue;
        }
        let Ok(pos) = f[1].parse::<u32>() else {
            malformed += 1;
            continue;
        };
        let reference = f[3];
        let alts: Vec<&str> = if f[4] == "." { Vec::new() } else { f[4].split(',').collect() };
        let info = f[7];
        let info_get =
            |key: &str| info.split(';').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v));
        let format: Vec<&str> = f[8].split(':').collect();
        let values: Vec<&str> = f[h.sample_col].split(':').collect();
        let fmt_get = |key: &str| format.iter().position(|k| *k == key).and_then(|i| values.get(i).copied());
        let gt = fmt_get("GT").unwrap_or(".");
        let (genotype, zyg) = genotype_letters(gt, reference, &alts);
        let only_symbolic = alts.iter().all(|a| is_symbolic(a));
        let end_info = info_get("END").and_then(|v| v.parse::<u32>().ok());
        let ref_block = only_symbolic && (end_info.is_some() || !alts.is_empty()) && zyg != Zygosity::HomAlt;
        let end = end_info.unwrap_or(pos + (reference.len() as u32).saturating_sub(1)).max(pos);
        ref_blocks += u64::from(ref_block);
        let depth =
            fmt_get("DP").or_else(|| fmt_get("MIN_DP")).or_else(|| info_get("DP")).and_then(|v| v.parse::<u32>().ok());
        let rsid = f[2].split(';').find(|i| i.starts_with("rs")).map(str::to_string);
        sink(Call {
            chrom: normalize_chrom(f[0]),
            pos,
            end,
            rsid,
            reference: Some(reference.to_string()),
            alt: alts.iter().filter(|a| !a.starts_with('<')).map(|a| a.to_string()).collect(),
            genotype,
            gt: Some(gt.to_string()),
            zygosity: Some(zyg),
            filter: Some(f[6].to_string()).filter(|v| v != "."),
            quality: f[5].parse::<f32>().ok(),
            depth,
            ref_block,
        })?;
        records += 1;
    }
    if malformed > 0 {
        warnings.push(format!("skipped {malformed} malformed VCF records"));
    }
    if h.build == Build::Unknown {
        warnings.push("could not determine the genome build (no chr1 contig length or reference header)".into());
    }
    let gvcf = h.gvcf || ref_blocks > 0;
    let _ = records;
    Ok(SourceInfo {
        source_format: if gvcf { "gvcf".into() } else { "vcf".into() },
        assay: "wgs",
        build: h.build,
        build_evidence: h.evidence,
        sample: Some(h.sample),
        chip: None,
        warnings,
        contigs: h.contigs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(genotype_letters("0/1", "T", &["C"]), ("TC".into(), Zygosity::Het));
        assert_eq!(genotype_letters("1|1", "T", &["C"]), ("CC".into(), Zygosity::HomAlt));
        assert_eq!(genotype_letters("0/0", "T", &["<NON_REF>"]), ("TT".into(), Zygosity::HomRef));
        assert_eq!(genotype_letters("1", "A", &["G"]), ("G".into(), Zygosity::Hemi));
        assert_eq!(genotype_letters("./.", "A", &["G"]).1, Zygosity::NoCall);
        assert_eq!(genotype_letters("0/1", "AT", &["A"]), ("AT/A".into(), Zygosity::Het));
        assert_eq!(genotype_letters("1/2", "A", &["G", "T"]), ("GT".into(), Zygosity::Het));
    }

    #[test]
    fn build_by_contig_length_and_gvcf() {
        let vcf = "##fileformat=VCFv4.2\n##contig=<ID=chr1,length=248956422>\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\tS1\n\
                   chr1\t100\t.\tA\t<NON_REF>\t.\t.\tEND=200\tGT:DP\t0/0:30\nchr1\t201\trs5\tA\tG,<NON_REF>\t50\tPASS\t.\tGT:DP\t0/1:20\n";
        let mut calls = Vec::new();
        let info = parse(Box::new(std::io::Cursor::new(vcf.to_string())), None, &mut |c| {
            calls.push(c);
            Ok(())
        })
        .unwrap();
        assert_eq!((info.build, info.build_evidence), (Build::GRCh38, "contig-lengths"));
        assert_eq!(info.source_format, "gvcf");
        assert!(calls[0].ref_block);
        assert_eq!(calls[0].end, 200);
        assert_eq!(calls[1].alt, vec!["G".to_string()]);
        assert_eq!(calls[1].rsid.as_deref(), Some("rs5"));
        assert_eq!(calls[1].depth, Some(20));
    }
}
