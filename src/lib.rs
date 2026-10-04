//! genome-cli: personal genomic data (array exports, WGS VCFs, FASTQ reads)
//! normalized into one genotype model.

#![recursion_limit = "512"]

pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod db;
pub mod error;
pub mod fasta;
pub mod fetch;
pub mod gtstore;
pub mod liftover;
pub mod model;
pub mod output;
pub mod parse;
pub mod pipeline;
pub mod rsids;
pub mod store;
pub mod summary;
pub mod util;

use clap::Parser;

use crate::cli::Command;
use crate::error::{AppError, ErrorKind};
use crate::parse::InputFormat;

/// `import --format 23andme` and `export --format vcf` reuse the global
/// (repeatable) `--format` flag: split such values from the output format.
/// Returns true when export should write VCF.
fn reroute_format(cli: &mut cli::Cli) -> Result<bool, AppError> {
    let mut vcf = false;
    let mut keep = Vec::new();
    for f in std::mem::take(&mut cli.global.format) {
        let lower = f.to_ascii_lowercase();
        match &mut cli.command {
            Command::Import(a) if InputFormat::parse(&lower).is_some() => {
                a.input_format = InputFormat::parse(&lower).expect("checked");
            }
            Command::Export(_) if lower == "vcf" => vcf = true,
            _ => {
                output::Format::parse(&f)?;
                keep.push(f);
            }
        }
    }
    cli.global.format = keep;
    Ok(vcf)
}

/// Parse arguments, run the command and return the process exit code.
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let mut cli = match cli::Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { ErrorKind::Usage.exit_code() } else { 0 };
        }
    };
    let wants_json =
        |f: &[String]| f.last().is_some_and(|f| matches!(f.to_ascii_lowercase().as_str(), "json" | "jsonl"));
    let mut json_errors = wants_json(&cli.global.format);
    let result = reroute_format(&mut cli).and_then(|export_vcf| {
        let ctx = context::Ctx::new(&cli.global)?;
        json_errors = ctx.out.format.is_json();
        commands::dispatch(&ctx, cli.command, export_vcf)
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            if json_errors {
                println!("{}", output::error_envelope(&e));
            }
            eprintln!("genome: error: {e}");
            e.exit_code()
        }
    }
}
