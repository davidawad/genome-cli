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

pub fn run(ctx: &Ctx, a: CompareArgs) -> Result<()> {
    let db = ctx.db()?;
    let (ka, kb) = (store::get(&db, &a.a)?, store::get(&db, &a.b)?);
    drop(db);
    let cap = a.max_discordant.unwrap_or_else(|| ctx.get("max_discordant").parse().unwrap_or(50));
    let region = a
        .region
        .as_deref()
        .map(|r| Region::parse(r).ok_or_else(|| AppError::usage(format!("bad region '{r}'"))))
        .transpose()?;
    let (build_a, build_b) = (ka.build(), kb.build());
    if build_a != build_b && (build_a == Build::Unknown || build_b == Build::Unknown) {
        return Err(AppError::invalid(format!(
            "cannot compare {} ({}) with {} ({}): unknown build",
            ka.id, ka.build, kb.id, kb.build
        )));
    }
    let mut lifter = ctx.lifter();
    let (mut ra, mut rb) = (ctx.resolver(), ctx.resolver());
    let mut va = KitView::open(ctx, ka, &mut ra)?;
    let mut vb = KitView::open(ctx, kb, &mut rb)?;
    let mut warnings = Vec::new();
    let lifted = build_a != build_b;
    if lifted {
        warnings.push(format!("{} lifted from {} to {} for comparison", vb.kit.id, build_b.as_str(), build_a.as_str()));
    }

    // B's calls in A's coordinates.
    let mut b_sites: HashMap<(String, u32), (Call, Lift)> = HashMap::new();
    let mut unmapped = 0u64;
    let mut b_calls = Vec::new();
    vb.reader.for_each(&mut |c| {
        if c.is_called() && !c.ref_block {
            b_calls.push(c);
        }
        Ok(true)
    })?;
    for c in b_calls {
        let (mut c, from) = if lifted {
            match lifter.lift(build_b, build_a, &c.chrom, c.pos)? {
                Some(l) => {
                    let orig = c.pos;
                    let mut c = c;
                    if l.reverse {
                        c.genotype = complement(&c.genotype);
                        c.reference = c.reference.as_deref().map(complement);
                        c.alt = c.alt.iter().map(|x| complement(x)).collect();
                    }
                    c.chrom = l.chrom;
                    c.pos = l.pos;
                    (c, Some((build_b, orig)))
                }
                None => {
                    unmapped += 1;
                    continue;
                }
            }
        } else {
            (c, None)
        };
        if region.as_ref().is_some_and(|r| !r.contains(&c.chrom, c.pos)) {
            continue;
        }
        c.end = c.pos;
        b_sites.insert((c.chrom.clone(), c.pos), (c, from));
    }
    if unmapped > 0 {
        warnings.push(format!("{unmapped} sites of {} did not lift over", vb.kit.id));
    }

    let (mut overlap, mut concordant, mut inferred_sites, mut unresolved) = (0u64, 0u64, 0u64, 0u64);
    let mut discordant_sites = Vec::new();
    let mut discordant = 0u64;
    let (id_a, id_b) = (va.kit.id.clone(), vb.kit.id.clone());
    let mut judge =
        |ca: &Call, src_a: &str, cb: &Call, src_b: &str, from_b: Option<(Build, u32)>| match (snv_key(ca), snv_key(cb))
        {
            (Some(x), Some(y)) => {
                overlap += 1;
                if x == y {
                    concordant += 1;
                } else {
                    discordant += 1;
                    if discordant_sites.len() < cap {
                        discordant_sites.push(json!({
                            "chrom": ca.chrom, "pos": ca.pos, "rsid": ca.rsid.clone().or_else(|| cb.rsid.clone()),
                            "a": genotype_row(&id_a, ca, build_a, src_a, None),
                            "b": genotype_row(&id_b, cb, build_a, src_b, from_b),
                        }));
                    }
                }
            }
            _ => unresolved += 1,
        };

    let mut a_calls = Vec::new();
    va.reader.for_each(&mut |c| {
        if c.is_called() && !c.ref_block && region.as_ref().is_none_or(|r| r.contains(&c.chrom, c.pos)) {
            a_calls.push(c);
        }
        Ok(true)
    })?;
    for ca in a_calls {
        let ca = va.enrich_array(ca)?;
        if let Some((cb, from)) = b_sites.remove(&(ca.chrom.clone(), ca.pos)) {
            judge(&ca, "observed", &cb, "observed", from);
        } else if vb.kit.absent_means_ref() && is_primary(&ca.chrom) {
            // B is a variant-only WGS VCF: absent site = homozygous reference.
            let reference = match ca.reference.clone().filter(|r| r.len() == 1) {
                Some(r) => Some(r),
                None => va.reference_at(&ca.chrom, ca.pos, None)?,
            };
            inferred_sites += 1;
            judge(&ca, "observed", &inferred(&ca, reference), "inferred_ref", None);
        }
    }
    if va.kit.absent_means_ref() {
        let rest: Vec<(Call, Lift)> = b_sites.into_values().filter(|(c, _)| is_primary(&c.chrom)).collect();
        for (cb, from) in rest {
            let reference = match cb.reference.clone().filter(|r| r.len() == 1) {
                Some(r) => Some(r),
                None => va.reference_at(&cb.chrom, cb.pos, None)?,
            };
            inferred_sites += 1;
            judge(&inferred(&cb, reference), "inferred_ref", &cb, "observed", from);
        }
    }
    if inferred_sites > 0 {
        warnings.push(INFERRED_REF_WARNING.into());
    }
    if unresolved > 0 {
        warnings.push(format!(
            "{unresolved} overlapping sites skipped: indel codes or unknown reference allele (configure reference_{} for inferred_ref letters)",
            build_a.as_str().to_ascii_lowercase()
        ));
    }
    if discordant as usize > discordant_sites.len() {
        warnings.push(format!("discordant_sites capped at {cap} of {discordant}"));
    }
    warnings.extend(va.warnings.drain(..).chain(vb.warnings.drain(..)));
    let row = to_record(&json!({
        "a": id_a,
        "b": id_b,
        "build": build_a.as_str(),
        "overlap": overlap,
        "concordant": concordant,
        "discordant": discordant,
        "concordance": if overlap > 0 { ((concordant as f64 / overlap as f64) * 1e6).round() / 1e6 } else { 0.0 },
        "discordant_sites": discordant_sites,
        "inferred_ref_sites": inferred_sites,
        "skipped_sites": unresolved,
        "lifted": lifted,
    }));
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
