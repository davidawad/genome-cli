//! `genome compare`: genotype concordance between two kits.

use std::collections::HashMap;

use serde_json::json;

use crate::cli::CompareArgs;
use crate::commands::genotype_row;
use crate::commands::lookup::{dedup, KitView, INFERRED_REF_WARNING};
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::model::{allele_key, complement, is_primary, Build, Call, Region, Zygosity};
use crate::output::{to_record, Report};
use crate::store;

/// Original position of a call lifted from B's build.
type Lift = Option<(Build, u32)>;

/// Genotype comparable as unordered A/C/G/T alleles (skips indel codes like `DI`).
fn snv_key(c: &Call) -> Option<Vec<String>> {
    if !c.is_called() || c.genotype.contains('/') || !c.genotype.chars().all(|b| matches!(b, 'A' | 'C' | 'G' | 'T')) {
        return None;
    }
    allele_key(&c.genotype)
}

fn inferred(c: &Call, reference: Option<String>) -> Call {
    Call {
        chrom: c.chrom.clone(),
        pos: c.pos,
        end: c.pos,
        rsid: c.rsid.clone(),
        genotype: reference.as_ref().map(|r| format!("{r}{r}")).unwrap_or_default(),
        reference,
        zygosity: Some(Zygosity::HomRef),
        ..Call::default()
    }
}

/// Move a call lifted onto the other strand to the + strand of the target build.
fn apply_lift(mut c: Call, chrom: String, pos: u32, reverse: bool) -> Call {
    if reverse {
        c.genotype = complement(&c.genotype);
        c.reference = c.reference.as_deref().map(complement);
        c.alt = c.alt.iter().map(|x| complement(x)).collect();
    }
    c.chrom = chrom;
    c.pos = pos;
    c
}

/// B's called sites keyed in A's coordinates (lifted when the builds differ),
/// plus how many failed to lift.
fn b_sites_in_a(
    ctx: &Ctx,
    vb: &mut KitView,
    build_a: Build,
    region: Option<&Region>,
) -> Result<(HashMap<(String, u32), (Call, Lift)>, u64)> {
    let build_b = vb.build();
    let mut lifter = ctx.lifter();
    let mut b_calls = Vec::new();
    vb.reader.for_each(&mut |c| {
        if c.is_called() && !c.ref_block {
            b_calls.push(c);
        }
        Ok(true)
    })?;
    let mut sites = HashMap::new();
    let mut unmapped = 0u64;
    for c in b_calls {
        let (mut c, from) = if build_b == build_a {
            (c, None)
        } else if let Some(l) = lifter.lift(build_b, build_a, &c.chrom, c.pos)? {
            let orig = c.pos;
            (apply_lift(c, l.chrom, l.pos, l.reverse), Some((build_b, orig)))
        } else {
            unmapped += 1;
            continue;
        };
        if region.is_some_and(|r| !r.contains(&c.chrom, c.pos)) {
            continue;
        }
        c.end = c.pos;
        sites.insert((c.chrom.clone(), c.pos), (c, from));
    }
    Ok((sites, unmapped))
}

/// Single-letter reference at a call's site: its own, else the kit's FASTA.
fn reference_for(view: &mut KitView, c: &Call) -> Result<Option<String>> {
    match c.reference.clone().filter(|r| r.len() == 1) {
        Some(r) => Ok(Some(r)),
        None => view.reference_at(&c.chrom, c.pos, None),
    }
}

/// Running concordance counts for one comparison.
struct Tally {
    id_a: String,
    id_b: String,
    build: Build,
    cap: usize,
    overlap: u64,
    concordant: u64,
    discordant: u64,
    inferred: u64,
    unresolved: u64,
    discordant_sites: Vec<serde_json::Value>,
}

impl Tally {
    fn new(id_a: String, id_b: String, build: Build, cap: usize) -> Self {
        Tally {
            id_a,
            id_b,
            build,
            cap,
            overlap: 0,
            concordant: 0,
            discordant: 0,
            inferred: 0,
            unresolved: 0,
            discordant_sites: Vec::new(),
        }
    }

    fn judge(&mut self, ca: &Call, src_a: &str, cb: &Call, src_b: &str, from_b: Lift) {
        let (Some(x), Some(y)) = (snv_key(ca), snv_key(cb)) else {
            self.unresolved += 1;
            return;
        };
        self.overlap += 1;
        if x == y {
            self.concordant += 1;
            return;
        }
        self.discordant += 1;
        if self.discordant_sites.len() < self.cap {
            self.discordant_sites.push(json!({
                "chrom": ca.chrom, "pos": ca.pos, "rsid": ca.rsid.clone().or_else(|| cb.rsid.clone()),
                "a": genotype_row(&self.id_a, ca, self.build, src_a, None),
                "b": genotype_row(&self.id_b, cb, self.build, src_b, from_b),
            }));
        }
    }

    fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.inferred > 0 {
            w.push(INFERRED_REF_WARNING.into());
        }
        if self.unresolved > 0 {
            w.push(format!(
                "{} overlapping sites skipped: indel codes or unknown reference allele (configure reference_{} for inferred_ref letters)",
                self.unresolved,
                self.build.as_str().to_ascii_lowercase()
            ));
        }
        if self.discordant as usize > self.discordant_sites.len() {
            w.push(format!("discordant_sites capped at {} of {}", self.cap, self.discordant));
        }
        w
    }

    fn record(self, lifted: bool) -> crate::output::Record {
        let concordance = if self.overlap > 0 {
            ((self.concordant as f64 / self.overlap as f64) * 1e6).round() / 1e6
        } else {
            0.0
        };
        to_record(&json!({
            "a": self.id_a,
            "b": self.id_b,
            "build": self.build.as_str(),
            "overlap": self.overlap,
            "concordant": self.concordant,
            "discordant": self.discordant,
            "concordance": concordance,
            "discordant_sites": self.discordant_sites,
            "inferred_ref_sites": self.inferred,
            "skipped_sites": self.unresolved,
            "lifted": lifted,
        }))
    }
}

/// Compare every called A site against B (observed, or inferred hom-ref when B is variant-only).
fn judge_a_sites(
    va: &mut KitView,
    b_absent_means_ref: bool,
    b_sites: &mut HashMap<(String, u32), (Call, Lift)>,
    region: Option<&Region>,
    t: &mut Tally,
) -> Result<()> {
    let mut a_calls = Vec::new();
    va.reader.for_each(&mut |c| {
        if c.is_called() && !c.ref_block && region.is_none_or(|r| r.contains(&c.chrom, c.pos)) {
            a_calls.push(c);
        }
        Ok(true)
    })?;
    for ca in a_calls {
        let ca = va.enrich_array(ca)?;
        if let Some((cb, from)) = b_sites.remove(&(ca.chrom.clone(), ca.pos)) {
            t.judge(&ca, "observed", &cb, "observed", from);
        } else if b_absent_means_ref && is_primary(&ca.chrom) {
            let reference = reference_for(va, &ca)?;
            t.inferred += 1;
            t.judge(&ca, "observed", &inferred(&ca, reference), "inferred_ref", None);
        }
    }
    Ok(())
}

/// When A is variant-only, B sites A never listed are A hom-ref.
fn judge_b_only_sites(va: &mut KitView, b_sites: HashMap<(String, u32), (Call, Lift)>, t: &mut Tally) -> Result<()> {
    for (cb, from) in b_sites.into_values().filter(|(c, _)| is_primary(&c.chrom)) {
        let reference = reference_for(va, &cb)?;
        t.inferred += 1;
        t.judge(&inferred(&cb, reference), "inferred_ref", &cb, "observed", from);
    }
    Ok(())
}

fn check_builds(ka: &store::Kit, kb: &store::Kit) -> Result<()> {
    let (build_a, build_b) = (ka.build(), kb.build());
    if build_a != build_b && (build_a == Build::Unknown || build_b == Build::Unknown) {
        return Err(AppError::invalid(format!(
            "cannot compare {} ({}) with {} ({}): unknown build",
            ka.id, ka.build, kb.id, kb.build
        )));
    }
    Ok(())
}

pub fn run(ctx: &Ctx, a: CompareArgs) -> Result<()> {
    let db = ctx.db()?;
    let (ka, kb) = (store::get(&db, &a.a)?, store::get(&db, &a.b)?);
    drop(db);
    check_builds(&ka, &kb)?;
    let cap = a.max_discordant.unwrap_or_else(|| ctx.get("max_discordant").parse().unwrap_or(50));
    let region = a
        .region
        .as_deref()
        .map(|r| Region::parse(r).ok_or_else(|| AppError::usage(format!("bad region '{r}'"))))
        .transpose()?;
    let (build_a, build_b) = (ka.build(), kb.build());
    let (mut ra, mut rb) = (ctx.resolver(), ctx.resolver());
    let mut va = KitView::open(ctx, ka, &mut ra)?;
    let mut vb = KitView::open(ctx, kb, &mut rb)?;
    let lifted = build_a != build_b;
    let mut warnings = Vec::new();
    if lifted {
        warnings.push(format!("{} lifted from {} to {} for comparison", vb.kit.id, build_b.as_str(), build_a.as_str()));
    }
    let (mut b_sites, unmapped) = b_sites_in_a(ctx, &mut vb, build_a, region.as_ref())?;
    if unmapped > 0 {
        warnings.push(format!("{unmapped} sites of {} did not lift over", vb.kit.id));
    }
    let mut t = Tally::new(va.kit.id.clone(), vb.kit.id.clone(), build_a, cap);
    judge_a_sites(&mut va, vb.kit.absent_means_ref(), &mut b_sites, region.as_ref(), &mut t)?;
    if va.kit.absent_means_ref() {
        judge_b_only_sites(&mut va, b_sites, &mut t)?;
    }
    warnings.extend(t.warnings());
    warnings.extend(va.warnings.drain(..).chain(vb.warnings.drain(..)));
    let row = t.record(lifted);
    ctx.emit(
        &Report::new("compare", vec![row])
            .table_columns(&[
                "a",
                "b",
                "build",
                "overlap",
                "concordant",
                "discordant",
                "concordance",
                "inferred_ref_sites",
            ])
            .warnings(dedup(warnings)),
    )
}
