//! `genome liftover`.

use serde_json::json;

use crate::cli::LiftoverArgs;
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::liftover::{self, Chain, CHAINS};
use crate::model::{parse_locus, Build};
use crate::output::{to_record, Report};

pub fn run(ctx: &Ctx, a: LiftoverArgs) -> Result<()> {
    if a.fetch {
        let rows = CHAINS
            .iter()
            .map(|s| {
                let p =
                    liftover::ensure_chain(&ctx.cache_dir, ctx.get("ucsc_url"), s, ctx.offline(), &|m| ctx.info(m))?;
                let sha = liftover::sha256_file(&p)?;
                Ok(to_record(&json!({
                    "from": s.from.as_str(), "to": s.to.as_str(), "path": p, "sha256": sha, "verified": sha == s.sha256,
                })))
            })
            .collect::<Result<Vec<_>>>()?;
        if a.loci.is_empty() {
            return ctx.emit(&Report::new("liftover-chains", rows));
        }
    }
    if a.loci.is_empty() {
        return Err(AppError::usage("give positions CHR:POS to lift (or --fetch to cache the chain files)"));
    }
    let from = Build::parse(&a.from)
        .filter(|b| *b != Build::Unknown)
        .ok_or_else(|| AppError::usage(format!("unknown build '{}'", a.from)))?;
    let to = match a.to.as_deref() {
        Some(t) => Build::parse(t)
            .filter(|b| *b != Build::Unknown)
            .ok_or_else(|| AppError::usage(format!("unknown build '{t}'")))?,
        None => from.other(),
    };
    let mut lifter = ctx.lifter();
    if let Some(p) = &a.chain {
        lifter.set_chain(from, to, Chain::load(p)?);
    }
    let mut warnings = Vec::new();
    let mut rows = Vec::new();
    for l in &a.loci {
        let (chrom, pos) =
            parse_locus(l).ok_or_else(|| AppError::usage(format!("bad position '{l}' (expected CHR:POS)")))?;
        let hit = if from == to {
            Some(liftover::Lifted { chrom: chrom.clone(), pos, reverse: false })
        } else {
            lifter.lift(from, to, &chrom, pos)?
        };
        if hit.is_none() {
            warnings.push(format!("{chrom}:{pos} does not map to {}", to.as_str()));
        }
        rows.push(to_record(&json!({
            "chrom": chrom,
            "pos": pos,
            "build": from.as_str(),
            "lifted_chrom": hit.as_ref().map(|h| h.chrom.clone()),
            "lifted_pos": hit.as_ref().map(|h| h.pos),
            "lifted_build": to.as_str(),
            "strand": hit.as_ref().map(|h| if h.reverse { "-" } else { "+" }),
            "status": if hit.is_some() { "mapped" } else { "unmapped" },
        })));
    }
    ctx.emit(&Report::new("liftover", rows).warnings(warnings))
}
