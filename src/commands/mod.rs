//! Command implementations.

pub mod compare;
pub mod config_cmd;
pub mod db_cmd;
pub mod doctor;
pub mod export;
pub mod import;
pub mod key_cmd;
pub mod kits;
pub mod liftover_cmd;
pub mod lookup;
pub mod misc;
pub mod rsid_table;

use serde_json::{json, Value};

use crate::cli::Command;
use crate::context::Ctx;
use crate::error::Result;
use crate::model::{Build, Call};
use crate::output::Record;

/// Process exit status requested by a successful command.
pub type Status = i32;

pub fn dispatch(ctx: &Ctx, cmd: Command, export_vcf: bool) -> Result<Status> {
    match cmd {
        Command::Import(a) => import::run(ctx, a).map(|_| 0),
        Command::Kits => kits::list(ctx).map(|()| 0),
        Command::Rm(a) => kits::rm(ctx, a).map(|()| 0),
        Command::Summary(a) => kits::summary(ctx, a).map(|()| 0),
        Command::Lookup(a) => lookup::run(ctx, a).map(|()| 0),
        Command::Liftover(a) => liftover_cmd::run(ctx, a).map(|()| 0),
        Command::RsidTable(a) => rsid_table::run(ctx, a).map(|()| 0),
        Command::Compare(a) => compare::run(ctx, a).map(|()| 0),
        Command::Export(a) => export::run(ctx, a, export_vcf).map(|()| 0),
        Command::Pipeline(c) => crate::pipeline::run_cmd(ctx, c),
        Command::Doctor => doctor::run(ctx).map(|()| 0),
        Command::Db(c) => db_cmd::run(ctx, c).map(|()| 0),
        Command::Key(c) => key_cmd::run(ctx, c).map(|()| 0),
        Command::Audit(c) => db_cmd::audit(ctx, c).map(|()| 0),
        Command::Decrypt(a) => db_cmd::decrypt(ctx, a).map(|()| 0),
        Command::Config(c) => config_cmd::run(ctx, c).map(|()| 0),
        Command::Completions(a) => misc::completions(a).map(|()| 0),
        Command::Man(a) => misc::man(a).map(|()| 0),
    }
}

fn round2(q: f32) -> f64 {
    (f64::from(q) * 100.0).round() / 100.0
}

/// A `genotypes` record (genome/v1).
pub fn genotype_row(kit: &str, c: &Call, build: Build, call_source: &str, lifted_from: Option<(Build, u32)>) -> Record {
    let called = !matches!(call_source, "missing");
    let mut r = Record::new();
    r.insert("kit".into(), json!(kit));
    r.insert("rsid".into(), json!(c.rsid));
    r.insert("chrom".into(), if c.chrom.is_empty() { Value::Null } else { json!(c.chrom) });
    r.insert("pos".into(), if c.pos == 0 { Value::Null } else { json!(c.pos) });
    r.insert("build".into(), json!(build.as_str()));
    r.insert("ref".into(), json!(c.reference));
    r.insert("alt".into(), json!(c.alt));
    r.insert("genotype".into(), if called && !c.genotype.is_empty() { json!(c.genotype) } else { Value::Null });
    r.insert("zygosity".into(), json!(c.zyg().as_str()));
    r.insert("call_source".into(), json!(call_source));
    r.insert("filter".into(), json!(c.filter));
    r.insert("quality".into(), json!(c.quality.map(round2)));
    r.insert("depth".into(), json!(c.depth));
    r.insert("lifted_from".into(), lifted_from.map_or(Value::Null, |(b, p)| json!({"build": b.as_str(), "pos": p})));
    r
}

pub const GENOTYPE_TABLE_COLUMNS: &[&str] = &[
    "kit",
    "rsid",
    "chrom",
    "pos",
    "build",
    "ref",
    "alt",
    "genotype",
    "zygosity",
    "call_source",
    "filter",
    "quality",
    "depth",
];
