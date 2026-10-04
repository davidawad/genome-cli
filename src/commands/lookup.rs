//! `genome lookup`: genotypes by rsid or position, applying ref_calls semantics.

use crate::cli::LookupArgs;
use crate::commands::{genotype_row, GENOTYPE_TABLE_COLUMNS};
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::fasta::Fasta;
use crate::gtstore::Reader;
use crate::model::{array_zygosity, is_primary, parse_locus, Build, Call, Zygosity};
use crate::output::{Record, Report};
use crate::rsids::Resolver;
use crate::store::{self, Kit};

/// Everything needed to answer genotype questions about one kit.
pub struct KitView<'a> {
    pub kit: Kit,
    pub reader: Reader,
    pub fasta: Option<Fasta>,
    pub resolver: &'a mut Resolver,
    pub warnings: Vec<String>,
}

pub const INFERRED_REF_WARNING: &str = "call_source inferred_ref: site absent from a variant-only WGS VCF \
    (ref_calls = absent-means-ref) and reported as homozygous reference because the assay covers the genome";

impl<'a> KitView<'a> {
    pub fn open(ctx: &Ctx, kit: Kit, resolver: &'a mut Resolver) -> Result<Self> {
        let reader = Reader::open(std::path::Path::new(&kit.store_dir))?;
        let fasta = ctx.reference_fasta(kit.build()).and_then(|p| Fasta::open(&p).ok());
        Ok(Self { kit, reader, fasta, resolver, warnings: Vec::new() })
    }

    pub fn build(&self) -> Build {
        self.kit.build()
    }

    /// Reference base(s) at a site: coordinate table first, then the reference FASTA.
    pub fn reference_at(&mut self, chrom: &str, pos: u32, hint: Option<&str>) -> Result<Option<String>> {
        if let Some(h) = hint {
            return Ok(Some(h.to_string()));
        }
        if let Some(c) = self.resolver.at(self.build(), chrom, pos)? {
            if c.reference.is_some() {
                return Ok(c.reference);
            }
        }
        match self.fasta.as_mut() {
            Some(f) => f.bases(chrom, pos, 1),
            None => Ok(None),
        }
    }

    /// Fill reference/alt and resolve hom_ref vs hom_alt for array calls.
    pub fn enrich_array(&mut self, mut c: Call) -> Result<Call> {
        if c.reference.is_none() && self.kit.assay == "array" {
            let coord = match c.rsid.clone() {
                Some(r) => self.resolver.resolve(&r, self.build())?.filter(|k| k.pos == c.pos && k.chrom == c.chrom),
                None => None,
            };
            let coord = match coord {
                Some(k) => Some(k),
                None => self.resolver.at(self.build(), &c.chrom, c.pos)?,
            };
            if let Some(k) = coord {
                c.reference = k.reference;
                c.alt = k.alt;
                if c.rsid.is_none() {
                    c.rsid = Some(k.rsid);
                }
            } else if let Some(f) = self.fasta.as_mut() {
                c.reference = f.bases(&c.chrom, c.pos, 1)?;
            }
            c.zygosity = Some(array_zygosity(&c.genotype, c.reference.as_deref()));
        }
        Ok(c)
    }

    /// Calls at a position with ref_calls semantics applied: (call, call_source).
    pub fn at(
        &mut self,
        chrom: &str,
        pos: u32,
        rsid: Option<&str>,
        ref_hint: Option<&str>,
        alt_hint: &[String],
    ) -> Result<Vec<(Call, &'static str)>> {
        let found = self.reader.at(chrom, pos)?;
        let mut out = Vec::new();
        for c in found {
            if c.ref_block || (c.pos != pos && c.zyg() == Zygosity::HomRef) {
                // gVCF reference block spanning the site: explicit hom_ref.
                let base = self.reference_at(chrom, pos, ref_hint)?;
                let call = Call {
                    chrom: chrom.to_string(),
                    pos,
                    end: pos,
                    rsid: rsid.map(str::to_string),
                    genotype: base.as_ref().map(|b| format!("{b}{b}")).unwrap_or_default(),
                    reference: base,
                    alt: alt_hint.to_vec(),
                    zygosity: Some(Zygosity::HomRef),
                    filter: c.filter.clone(),
                    quality: c.quality,
                    depth: c.depth,
                    ..Call::default()
                };
                out.push((call, "observed"));
            } else {
                let mut c = self.enrich_array(c)?;
                if c.rsid.is_none() {
                    c.rsid = rsid.map(str::to_string);
                }
                out.push((c, "observed"));
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
        let mut c = Call {
            chrom: chrom.to_string(),
            pos,
            end: pos,
            rsid: rsid.map(str::to_string),
            alt: alt_hint.to_vec(),
            zygosity: Some(Zygosity::NoCall),
            ..Call::default()
        };
        if self.kit.absent_means_ref() && is_primary(chrom) {
            let base = self.reference_at(chrom, pos, ref_hint)?;
            if base.is_none() {
                self.warnings.push(format!(
                    "{chrom}:{pos}: reference allele unknown (not in the rsid table; set reference_{} to a FASTA with .fai)",
                    self.build().as_str().to_ascii_lowercase()
                ));
            }
            c.genotype = base.as_ref().map(|b| format!("{b}{b}")).unwrap_or_default();
            c.reference = base;
            c.zygosity = Some(Zygosity::HomRef);
            self.warnings.push(INFERRED_REF_WARNING.into());
            return Ok(vec![(c, "inferred_ref")]);
        }
        c.reference = ref_hint.map(str::to_string);
        Ok(vec![(c, "missing")])
    }
}

pub fn run(ctx: &Ctx, a: LookupArgs) -> Result<()> {
    let db = ctx.db()?;
    let kit = store::get(&db, &a.kit)?;
    drop(db);
    let mut resolver = ctx.resolver();
    let mut lifter = ctx.lifter();
    let mut v = KitView::open(ctx, kit, &mut resolver)?;
    let kb = v.build();
    let kit_id = v.kit.id.clone();
    let mut rows: Vec<Record> = Vec::new();
    for rsid in &a.rsid {
        let rsid = rsid.trim();
        let found = if v.kit.rsid_records > 0 { v.reader.by_rsid(rsid)? } else { Vec::new() };
        if !found.is_empty() {
            for c in found {
                let c = v.enrich_array(c)?;
                rows.push(genotype_row(&kit_id, &c, kb, "observed", None));
            }
            continue;
        }
        // Resolve coordinates in the kit's build, or in the other build and lift.
        let mut lifted_from = None;
        let mut coord = if kb == Build::Unknown { None } else { v.resolver.resolve(rsid, kb)? };
        if coord.is_none() && kb != Build::Unknown {
            if let Some(o) = v.resolver.resolve(rsid, kb.other())? {
                match lifter.lift(kb.other(), kb, &o.chrom, o.pos) {
                    Ok(Some(l)) => {
                        lifted_from = Some((kb.other(), o.pos));
                        coord = Some(crate::rsids::Coord { chrom: l.chrom, pos: l.pos, reference: None, ..o });
                    }
                    Ok(None) => v.warnings.push(format!("{rsid}: does not lift over to {}", kb.as_str())),
                    Err(e) => v.warnings.push(format!("{rsid}: liftover failed: {e}")),
                }
            }
        }
        match coord {
            Some(k) => {
                if !v.kit.has_rsids {
                    v.warnings.push(format!(
                        "{rsid}: kit has no rsids; resolved via {} table to {}:{} ({})",
                        k.source,
                        k.chrom,
                        k.pos,
                        kb.as_str()
                    ));
                }
                for (c, src) in v.at(&k.chrom, k.pos, Some(rsid), k.reference.as_deref(), &k.alt)? {
                    rows.push(genotype_row(&kit_id, &c, kb, src, lifted_from));
                }
            }
            None => {
                v.warnings.push(format!(
                    "{rsid}: not in kit and no coordinates known (bundled table covers curated SNPs; \
                     run `genome rsid-table import DBSNP_VCF` for full backfill)"
                ));
                let c = Call { rsid: Some(rsid.to_string()), zygosity: Some(Zygosity::NoCall), ..Call::default() };
                rows.push(genotype_row(&kit_id, &c, kb, "missing", None));
            }
        }
    }
    let qbuild = match a.build.as_deref() {
        Some(b) => Build::parse(b).ok_or_else(|| AppError::usage(format!("unknown build '{b}'")))?,
        None => kb,
    };
    for p in &a.pos {
        let (chrom, pos) =
            parse_locus(p).ok_or_else(|| AppError::usage(format!("bad position '{p}' (expected CHR:POS)")))?;
        let (chrom, pos, lifted_from) = if qbuild != kb && qbuild != Build::Unknown && kb != Build::Unknown {
            match lifter.lift(qbuild, kb, &chrom, pos)? {
                Some(l) => (l.chrom, l.pos, Some((qbuild, pos))),
                None => {
                    v.warnings.push(format!("{p}: does not lift over from {} to {}", qbuild.as_str(), kb.as_str()));
                    let c = Call { chrom, pos, zygosity: Some(Zygosity::NoCall), ..Call::default() };
                    rows.push(genotype_row(&kit_id, &c, qbuild, "missing", None));
                    continue;
                }
            }
        } else {
            (chrom, pos, None)
        };
        let coord = v.resolver.at(kb, &chrom, pos)?;
        let (rsid, rf, alt) = match &coord {
            Some(k) => (Some(k.rsid.as_str()), k.reference.as_deref(), k.alt.clone()),
            None => (None, None, Vec::new()),
        };
        for (c, src) in v.at(&chrom, pos, rsid, rf, &alt)? {
            rows.push(genotype_row(&kit_id, &c, kb, src, lifted_from));
        }
    }
    let warnings = std::mem::take(&mut v.warnings);
    ctx.emit(&Report::new("genotypes", rows).table_columns(GENOTYPE_TABLE_COLUMNS).warnings(dedup(warnings)))
}

pub fn dedup(mut w: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    w.retain(|x| seen.insert(x.clone()));
    w
}
