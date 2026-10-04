//! Kit summaries: zygosity counts, per-chromosome counts and sex inference.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::gtstore::Site;
use crate::model::{by_chrom_keys, is_primary, Build, Zygosity};

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct Sex {
    pub call: String,
    pub x_het_rate: f64,
    pub y_call_rate: f64,
    pub method: String,
    pub x_sites: u64,
    pub y_sites: u64,
}

#[derive(Debug, Clone, Default)]
struct Counts {
    no_calls: u64,
    het: u64,
    hom_ref: u64,
    hom_alt: u64,
    hom: u64,
    hemi: u64,
    ref_blocks: u64,
    x_called: u64,
    x_het: u64,
    y_total: u64,
    y_called: u64,
}

/// Infer chromosomal sex from non-PAR X heterozygosity and Y calls.
pub fn infer_sex(assay: &str, x_called: u64, x_het: u64, y_total: u64, y_called: u64) -> Sex {
    let x_het_rate = if x_called > 0 { x_het as f64 / x_called as f64 } else { 0.0 };
    let array = assay == "array";
    let y_rate = if array {
        if y_total > 0 {
            y_called as f64 / y_total as f64
        } else {
            0.0
        }
    } else if x_called > 0 {
        y_called as f64 / x_called as f64
    } else {
        0.0
    };
    let enough_x = x_called >= 10;
    let call = if !enough_x {
        "uncertain"
    } else if array {
        match (x_het_rate, y_rate, y_total) {
            (h, y, t) if t > 0 && h <= 0.03 && y >= 0.5 => "male",
            (h, y, t) if h >= 0.08 && (t == 0 || y <= 0.2) => "female",
            _ => "uncertain",
        }
    } else {
        match (x_het_rate, y_rate) {
            (h, y) if h <= 0.25 && y >= 0.02 => "male",
            (h, y) if h >= 0.4 && y < 0.01 => "female",
            _ => "uncertain",
        }
    };
    let method = if array {
        "array: x_het_rate = het / called non-PAR X sites; y_call_rate = called / listed Y sites; \
         male if x_het_rate <= 0.03 and y_call_rate >= 0.5, female if x_het_rate >= 0.08 and y_call_rate <= 0.2"
    } else {
        "vcf: x_het_rate = het / called non-PAR X records; y_call_rate = called Y records / called non-PAR X records; \
         male if x_het_rate <= 0.25 and y_call_rate >= 0.02, female if x_het_rate >= 0.4 and y_call_rate < 0.01"
    };
    let round = |f: f64| (f * 10_000.0).round() / 10_000.0;
    Sex {
        call: call.into(),
        x_het_rate: round(x_het_rate),
        y_call_rate: round(y_rate),
        method: method.into(),
        x_sites: x_called,
        y_sites: y_total,
    }
}

/// Compute the `summary` record for a kit from its sorted sites.
pub fn summarize(
    kit: &str,
    sites: &[Site],
    contigs: &[String],
    build: Build,
    assay: &str,
    ref_calls: &str,
    source_format: &str,
) -> Map<String, Value> {
    let mut c = Counts::default();
    let mut chrom_counts = vec![0u64; contigs.len().max(26)];
    for s in sites {
        chrom_counts[s.contig as usize] += 1;
        c.add(s, build);
    }
    let by_chrom = fold_chroms(&chrom_counts, contigs);
    let sex = infer_sex(assay, c.x_called, c.x_het, c.y_total, c.y_called);
    let caveats = caveats(&c, &sex, build, assay, ref_calls, source_format);
    let mut m = Map::new();
    m.insert("kit".into(), kit.into());
    m.insert("records".into(), (sites.len() as u64).into());
    m.insert("no_calls".into(), c.no_calls.into());
    m.insert("het".into(), c.het.into());
    m.insert("hom_alt".into(), c.hom_alt.into());
    m.insert("hom_ref".into(), c.hom_ref.into());
    m.insert("hom_unknown_ref".into(), c.hom.into());
    m.insert("hemizygous".into(), c.hemi.into());
    m.insert("ref_blocks".into(), c.ref_blocks.into());
    m.insert("by_chrom".into(), Value::Object(by_chrom));
    m.insert("sex".into(), serde_json::to_value(&sex).unwrap_or(Value::Null));
    m.insert("caveats".into(), caveats.into());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn male_array_with_87_percent_y() {
        // 2 het of 400 non-PAR X, 26 of 30 Y sites called.
        let s = infer_sex("array", 400, 2, 1000, 870);
        assert_eq!(s.call, "male");
        assert!((s.y_call_rate - 0.87).abs() < 1e-9);
    }

    #[test]
    fn female_and_uncertain() {
        assert_eq!(infer_sex("array", 400, 100, 1000, 20).call, "female");
        assert_eq!(infer_sex("array", 400, 30, 1000, 500).call, "uncertain");
        assert_eq!(infer_sex("array", 5, 0, 10, 10).call, "uncertain");
        assert_eq!(infer_sex("wgs", 50_000, 2_000, 0, 3_000).call, "male");
        assert_eq!(infer_sex("wgs", 50_000, 30_000, 0, 100).call, "female");
    }
}
