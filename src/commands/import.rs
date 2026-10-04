//! `genome import`: detect, parse, normalize and store a kit.

use std::path::Path;

use crate::cli::ImportArgs;
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::gtstore::Writer;
use crate::model::Build;
use crate::output::{to_record, Report};
use crate::parse::{self, InputFormat};
use crate::store::{self, Kit};

/// Default kit name: the file name without genotype/compression extensions.
fn default_name(path: &Path) -> String {
    let mut name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "kit".into());
    for ext in [".gz", ".bgz", ".vcf", ".gvcf", ".g", ".txt", ".csv", ".tsv"] {
        if let Some(s) = name.strip_suffix(ext) {
            name = s.to_string();
        }
    }
    name
}

pub fn run(ctx: &Ctx, a: ImportArgs) -> Result<Kit> {
    if !a.file.exists() {
        return Err(AppError::not_found(format!("{} does not exist", a.file.display())));
    }
    let format = if a.input_format == InputFormat::Auto { parse::detect(&a.file)? } else { a.input_format };
    let db = ctx.db()?;
    let name = a.name.clone().unwrap_or_else(|| default_name(&a.file));
    if store::name_taken(&db, &name)? {
        if a.replace {
            let old = store::get(&db, &name)?;
            remove_kit(ctx, &db, &old)?;
        } else {
            return Err(AppError::invalid(format!("a kit named '{name}' already exists (use --name or --replace)")));
        }
    }
    let seq = store::next_seq(&db)?;
    let id = format!("k{seq}");
    let dir = ctx.kits_dir().join(&id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    ctx.info(&format!("importing {} as {} ({})", a.file.display(), id, format.as_str()));
    let result = (|| {
        let mut w = Writer::create(&dir)?;
        let mut rsid_records = 0i64;
        let mut n = 0u64;
        let info = parse::parse(&a.file, format, a.sample.as_deref(), &mut |c| {
            rsid_records += i64::from(c.rsid.as_deref().is_some_and(|r| r.starts_with("rs")));
            n += 1;
            if n.is_multiple_of(1_000_000) {
                ctx.verbose(&format!("{n} records"));
            }
            w.push(&c)
        })?;
        let (sites, contigs) = w.finish()?;
        let mut warnings = info.warnings.clone();
        let (build, evidence) = match a.build.as_deref() {
            Some(b) => {
                let b = Build::parse(b).ok_or_else(|| AppError::usage(format!("unknown build '{b}'")))?;
                if b != info.build && info.build != Build::Unknown {
                    warnings.push(format!(
                        "build overridden: file indicates {}, --build says {}",
                        info.build.as_str(),
                        b.as_str()
                    ));
                }
                (b, if b == info.build { info.build_evidence } else { "assumed" })
            }
            None => (info.build, info.build_evidence),
        };
        let assay = a.assay.clone().unwrap_or_else(|| info.assay.to_string());
        let source_format = if a.fastq_derived { "fastq-derived".to_string() } else { info.source_format.clone() };
        let ref_calls = a.ref_calls.clone().unwrap_or_else(|| {
            match (assay.as_str(), info.source_format.as_str()) {
                ("array", _) | (_, "gvcf") => "explicit",
                ("wgs", _) => "absent-means-ref",
                _ => "unknown",
            }
            .to_string()
        });
        let records = sites.len() as i64;
        let summary = crate::summary::summarize(&id, &sites, &contigs, build, &assay, &ref_calls, &info.source_format);
        let kit = Kit {
            seq,
            id: id.clone(),
            name: name.clone(),
            source_format,
            assay,
            build: build.as_str().to_string(),
            build_evidence: evidence.to_string(),
            sample: info.sample.clone().or_else(|| Some(name.clone())),
            records,
            has_rsids: records > 0 && rsid_records * 2 >= records,
            rsid_records,
            ref_calls,
            chip: info.chip.clone(),
            imported_at: crate::util::now_iso(),
            source_path: std::fs::canonicalize(&a.file)
                .unwrap_or_else(|_| a.file.clone())
                .to_string_lossy()
                .into_owned(),
            store_dir: dir.to_string_lossy().into_owned(),
            summary,
            warnings,
        };
        store::insert(&db, &kit)?;
        Ok(kit)
    })();
    let kit = match result {
        Ok(k) => k,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    ctx.info(&format!(
        "imported {} '{}': {} records, {} {} ({}), ref_calls {}",
        kit.id, kit.name, kit.records, kit.source_format, kit.build, kit.build_evidence, kit.ref_calls
    ));
    ctx.emit(&Report::new("kits", vec![to_record(&kit)]).warnings(kit.warnings.clone()).table_columns(KIT_COLUMNS))?;
    Ok(kit)
}

pub const KIT_COLUMNS: &[&str] =
    &["id", "name", "source_format", "assay", "build", "records", "has_rsids", "ref_calls", "sample"];

pub fn remove_kit(ctx: &Ctx, db: &crate::db::Db, kit: &Kit) -> Result<()> {
    store::delete(db, &kit.id)?;
    let dir = Path::new(&kit.store_dir);
    if dir.starts_with(ctx.kits_dir()) && dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(default_name(Path::new("/x/genome_Jane_v5_Full.txt")), "genome_Jane_v5_Full");
        assert_eq!(default_name(Path::new("sample.hard-filtered.vcf.gz")), "sample.hard-filtered");
        assert_eq!(default_name(Path::new("s.g.vcf.gz")), "s");
    }
}
