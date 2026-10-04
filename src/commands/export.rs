//! `genome export`: VCF, TSV or JSON.

use std::io::{BufWriter, Write};

use crate::cli::ExportArgs;
use crate::commands::lookup::KitView;
use crate::commands::{genotype_row, GENOTYPE_TABLE_COLUMNS};
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::model::{Build, Call, Region, Zygosity};
use crate::output::{Format, Report};
use crate::store;

fn vcf_contig(build: Build, chrom: &str) -> String {
    match (build, chrom) {
        (Build::GRCh38, "MT") => "chrM".into(),
        (Build::GRCh38, c) if crate::model::is_primary(c) => format!("chr{c}"),
        (_, c) => c.to_string(),
    }
}

/// VCF REF/ALT/GT for a call. Array calls without a known reference get REF=N.
fn vcf_fields(c: &Call) -> Option<(String, String, String)> {
    if c.ref_block {
        let r = c.reference.clone().unwrap_or_else(|| "N".into());
        return Some((r, "<*>".into(), c.gt.clone().unwrap_or_else(|| "0/0".into())));
    }
    if let (Some(r), Some(gt)) = (&c.reference, &c.gt) {
        let alt = if c.alt.is_empty() { ".".to_string() } else { c.alt.join(",") };
        return Some((r.clone(), alt, gt.clone()));
    }
    // Array genotype letters.
    let letters: Vec<String> = c.genotype.chars().map(|b| b.to_string()).collect();
    if letters.iter().any(|l| !matches!(l.as_str(), "A" | "C" | "G" | "T" | "-")) {
        return None;
    }
    let reference = c.reference.clone().unwrap_or_else(|| "N".into());
    if c.zyg() == Zygosity::NoCall {
        return Some((reference, ".".into(), if letters.len() == 1 { ".".into() } else { "./.".into() }));
    }
    let mut alts: Vec<String> = Vec::new();
    let idx: Vec<String> = letters
        .iter()
        .map(|l| {
            if *l == reference {
                "0".to_string()
            } else {
                if !alts.contains(l) {
                    alts.push(l.clone());
                }
                (alts.iter().position(|a| a == l).unwrap_or(0) + 1).to_string()
            }
        })
        .collect();
    let alt = if alts.is_empty() { ".".into() } else { alts.join(",") };
    Some((reference, alt, idx.join("/")))
}

pub fn run(ctx: &Ctx, a: ExportArgs, vcf: bool) -> Result<()> {
    let db = ctx.db()?;
    let kit = store::get(&db, &a.kit)?;
    drop(db);
    let regions = a
        .region
        .iter()
        .map(|r| Region::parse(r).ok_or_else(|| AppError::usage(format!("bad region '{r}' (CHR or CHR:START-END)"))))
        .collect::<Result<Vec<_>>>()?;
    let mut resolver = ctx.resolver();
    let mut v = KitView::open(ctx, kit, &mut resolver)?;
    let build = v.build();
    let id = v.kit.id.clone();
    // Collect calls (region-restricted when asked).
    let mut calls = Vec::new();
    if regions.is_empty() {
        v.reader.for_each(&mut |c| {
            calls.push(c);
            Ok(true)
        })?;
    } else {
        for r in &regions {
            v.reader.for_region(&r.chrom, r.start, r.end, &mut |c| {
                calls.push(c);
                Ok(())
            })?;
        }
    }
    let calls = calls
        .into_iter()
        .skip(a.offset)
        .take(a.limit.unwrap_or(usize::MAX))
        .map(|c| v.enrich_array(c))
        .collect::<Result<Vec<_>>>()?;
    let mut warnings = Vec::new();
    if v.kit.absent_means_ref() {
        warnings.push("variant-only VCF kit: sites not exported are implied homozygous reference".to_string());
    }
    ctx.audit(
        "export",
        serde_json::json!({
            "kit": id,
            "records": calls.len(),
            "format": if vcf { "vcf" } else { ctx.out.format.as_str() },
            "destination": match (&ctx.out.output, ctx.encrypt_output) {
                (_, true) => "encrypted",
                (Some(p), false) if p.as_os_str() != "-" => "plaintext-file",
                _ => "stdout",
            },
        }),
    )?;
    // Stream VCF to stdout; buffer it (in zeroized memory) for files and --encrypt-output.
    let to_stdout = ctx.out.output.as_ref().is_none_or(|p| p.as_os_str() == "-") && !ctx.encrypt_output;
    let mut buffered = zeroize::Zeroizing::new(Vec::new());
    let sink: Box<dyn Write + '_> =
        if to_stdout { Box::new(std::io::stdout().lock()) } else { Box::new(&mut *buffered) };
    let mut w = BufWriter::new(sink);
    let res: std::io::Result<()> = (|| {
        if vcf {
            writeln!(w, "##fileformat=VCFv4.2")?;
            writeln!(
                w,
                "##source=genome-cli {} export of {} ({})",
                env!("CARGO_PKG_VERSION"),
                id,
                v.kit.source_format
            )?;
            writeln!(w, "##reference={}", build.as_str())?;
            let mut seen = Vec::new();
            for c in &calls {
                if !seen.contains(&c.chrom) {
                    seen.push(c.chrom.clone());
                }
            }
            for c in &seen {
                writeln!(w, "##contig=<ID={}>", vcf_contig(build, c))?;
            }
            writeln!(w, "##INFO=<ID=END,Number=1,Type=Integer,Description=\"End of reference block\">")?;
            writeln!(w, "##INFO=<ID=REF_UNKNOWN,Number=0,Type=Flag,Description=\"Array site; reference allele unknown (REF=N)\">")?;
            writeln!(w, "##FORMAT=<ID=GT,Number=1,Type=String,Description=\"Genotype\">")?;
            writeln!(w, "##FORMAT=<ID=DP,Number=1,Type=Integer,Description=\"Read depth\">")?;
            let sample = v.kit.sample.clone().unwrap_or_else(|| v.kit.name.clone());
            writeln!(w, "#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\t{sample}")?;
            let mut skipped = 0;
            for c in &calls {
                let Some((r, alt, gt)) = vcf_fields(c) else {
                    skipped += 1;
                    continue;
                };
                let mut info = Vec::new();
                if c.ref_block {
                    info.push(format!("END={}", c.end));
                }
                if r == "N" {
                    info.push("REF_UNKNOWN".into());
                }
                writeln!(
                    w,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\tGT:DP\t{}:{}",
                    vcf_contig(build, &c.chrom),
                    c.pos,
                    c.rsid.as_deref().unwrap_or("."),
                    r,
                    alt,
                    c.quality.map_or(".".into(), |q| q.to_string()),
                    c.filter.as_deref().unwrap_or("."),
                    if info.is_empty() { ".".into() } else { info.join(";") },
                    gt,
                    c.depth.map_or(".".into(), |d| d.to_string()),
                )?;
            }
            if skipped > 0 {
                eprintln!(
                    "genome: warning: {skipped} array indel calls (I/D codes) not representable in VCF were skipped"
                );
            }
        }
        Ok(())
    })();
    match res {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        r => r?,
    }
    if vcf {
        w.flush().or_else(|e| if e.kind() == std::io::ErrorKind::BrokenPipe { Ok(()) } else { Err(e) })?;
        drop(w);
        return if to_stdout { Ok(()) } else { ctx.write_output(&buffered, true) };
    }
    drop(w);
    let rows = calls.iter().map(|c| genotype_row(&id, c, build, "observed", None)).collect();
    let mut out = ctx.out.clone();
    if out.format == Format::Table {
        out.format = Format::Tsv;
    }
    let report = Report::new("genotypes", rows).table_columns(GENOTYPE_TABLE_COLUMNS).warnings(warnings).exact();
    ctx.emit_with(&report, &out)
}
