//! `genome kits`, `genome rm`, `genome summary`.

use serde_json::Value;

use crate::cli::{RmArgs, SummaryArgs};
use crate::commands::import::{remove_kit, KIT_COLUMNS};
use crate::context::Ctx;
use crate::error::Result;
use crate::output::{to_record, Report};
use crate::store;

pub fn list(ctx: &Ctx) -> Result<()> {
    let db = ctx.db()?;
    let rows = store::list(&db)?.iter().map(to_record).collect();
    ctx.emit(&Report::new("kits", rows).table_columns(KIT_COLUMNS))
}

pub fn rm(ctx: &Ctx, a: RmArgs) -> Result<()> {
    let db = ctx.db()?;
    let kit = store::get(&db, &a.kit)?;
    remove_kit(ctx, &db, &kit)?;
    ctx.info(&format!("removed {} '{}'", kit.id, kit.name));
    ctx.emit(&Report::new("kits", vec![to_record(&kit)]).table_columns(KIT_COLUMNS))
}

pub fn summary(ctx: &Ctx, a: SummaryArgs) -> Result<()> {
    let db = ctx.db()?;
    let kits = if a.kits.is_empty() {
        store::list(&db)?
    } else {
        a.kits.iter().map(|k| store::get(&db, k)).collect::<Result<Vec<_>>>()?
    };
    let mut warnings = Vec::new();
    let rows = kits
        .into_iter()
        .map(|k| {
            warnings.extend(k.warnings.iter().map(|w| format!("{}: {w}", k.id)));
            let mut s = k.summary;
            // Older summaries may carry a different kit label; always report the id.
            s.insert("kit".into(), Value::from(k.id));
            s
        })
        .collect();
    let mut r = Report::new("summary", rows)
        .table_columns(&[
            "kit",
            "records",
            "no_calls",
            "het",
            "hom_alt",
            "hom_ref",
            "hom_unknown_ref",
            "hemizygous",
            "sex",
        ])
        .warnings(warnings);
    if ctx.out.format == crate::output::Format::Table {
        // Flatten sex to its call for the compact table.
        for row in &mut r.rows {
            if let Some(call) = row.get("sex").and_then(|s| s.get("call")).cloned() {
                row.insert("sex".into(), call);
            }
        }
    }
    ctx.emit(&r)
}
