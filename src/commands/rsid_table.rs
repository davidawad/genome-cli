//! `genome rsid-table` and `genome rsid-table import DBSNP_VCF`.

use serde_json::json;

use crate::cli::{RsidTableArgs, RsidTableCmd};
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::model::Build;
use crate::output::{to_record, Report};
use crate::rsids;

pub fn run(ctx: &Ctx, a: RsidTableArgs) -> Result<()> {
    match a.command {
        Some(RsidTableCmd::Import(i)) => {
            let build = i
                .build
                .as_deref()
                .map(|b| {
                    Build::parse(b)
                        .filter(|b| *b != Build::Unknown)
                        .ok_or_else(|| AppError::usage(format!("unknown build '{b}'")))
                })
                .transpose()?;
            ctx.info(&format!("indexing {} into {}", i.vcf.display(), ctx.cache_dir.join("dbsnp").display()));
            let (build, kept, skipped) =
                rsids::import_dbsnp(&i.vcf, &ctx.cache_dir, build, i.chunk.max(1), &|m| ctx.verbose(m))?;
            let (by_rs, by_pos) = rsids::dbsnp_paths(&ctx.cache_dir, build);
            ctx.info(&format!(
                "indexed {kept} rsids for {} ({skipped} records on non-primary contigs skipped)",
                build.as_str()
            ));
            ctx.emit(&Report::new(
                "rsid-table-import",
                vec![to_record(&json!({
                    "build": build.as_str(), "rsids": kept, "skipped": skipped, "by_rsid": by_rs, "by_pos": by_pos,
                }))],
            ))
        }
        None => {
            let rows = rsids::curated()
                .into_iter()
                .filter(|r| a.rsid.is_empty() || a.rsid.contains(&r.rsid))
                .map(|r| to_record(&r))
                .collect();
            let mut resolver = ctx.resolver();
            let dbsnp: Vec<&str> = [Build::GRCh37, Build::GRCh38]
                .into_iter()
                .filter(|b| resolver.has_dbsnp(*b))
                .map(Build::as_str)
                .collect();
            ctx.emit(
                &Report::new("rsid-table", rows)
                    .table_columns(&[
                        "rsid",
                        "gene",
                        "chrom",
                        "grch37_pos",
                        "grch37_ref",
                        "grch37_alt",
                        "grch38_pos",
                        "grch38_ref",
                        "grch38_alt",
                    ])
                    .meta("dbsnp_index", json!(dbsnp)),
            )
        }
    }
}
