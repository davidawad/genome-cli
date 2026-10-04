//! Command-line interface definition (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "genome",
    version,
    about = "Personal genomic data: array exports, WGS VCFs and FASTQ reads in one genotype model",
    long_about = "Personal genomic data: genotyping-array exports (23andMe, AncestryDNA, MyHeritage, FTDNA), \
        whole-genome VCF/gVCF files and raw FASTQ reads, normalized into one genotype model.\n\n\
        Kit metadata lives in a local FrankenSQLite (fsqlite) database; genotypes live in sorted per-kit \
        index files. Every command can emit table, json, jsonl, csv or tsv output; JSON uses the versioned \
        `genome/v1` envelope documented in docs/json-schema.md.",
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 not found, 4 invalid data, 5 database, 6 io, 7 config, \
        8 network, 9 external tool, 10 crypto (missing/wrong key, tampered data)."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Args, Default)]
pub struct GlobalOpts {
    /// Config file (default: $XDG_CONFIG_HOME/genome-cli/config.toml; env GENOME_CONFIG)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Data directory for the kit database and genotype stores (env GENOME_DATA_DIR)
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Kit metadata database file (env GENOME_DB)
    #[arg(long, global = true, value_name = "FILE")]
    pub db: Option<PathBuf>,
    /// Download cache for chain files, reference genome, dbSNP index (env GENOME_CACHE_DIR)
    #[arg(long, global = true, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,
    /// Output format: table, json, jsonl, csv, tsv (`import`: also the input format; `export`: vcf|tsv|json)
    #[arg(short = 'f', long, global = true, value_name = "FORMAT", action = clap::ArgAction::Append)]
    pub format: Vec<String>,
    /// Write output to FILE instead of stdout
    #[arg(short = 'o', long, global = true, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Decimal places for table/csv/tsv output
    #[arg(long, global = true, value_name = "N")]
    pub precision: Option<String>,
    /// Colour: auto, always, never
    #[arg(long, global = true, value_name = "WHEN")]
    pub color: Option<String>,
    /// CSV delimiter character (or 'tab')
    #[arg(long, global = true, value_name = "CHAR")]
    pub delimiter: Option<String>,
    /// Omit the header row in csv/tsv output
    #[arg(long, global = true)]
    pub no_header: bool,
    /// Text for missing values in table/csv/tsv output
    #[arg(long, global = true, value_name = "TEXT")]
    pub null: Option<String>,
    /// Comma-separated list of columns to output (table/csv/tsv)
    #[arg(long, global = true, value_name = "COLS", value_delimiter = ',')]
    pub columns: Option<Vec<String>>,
    /// Never download (chain files, reference); fail if not cached
    #[arg(long, global = true)]
    pub offline: bool,
    /// Store personal data UNENCRYPTED (new database) or open an unencrypted one; prints a warning
    #[arg(long, global = true)]
    pub insecure_plaintext: bool,
    /// Encrypt --output files with a passphrase (GENOME_EXPORT_KEY or prompt); read with `genome decrypt`
    #[arg(long, global = true)]
    pub encrypt_output: bool,
    /// Suppress informational messages
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// Print extra diagnostics to stderr
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Import an array export or (g)VCF as a kit
    Import(ImportArgs),
    /// List imported kits
    #[command(alias = "ls")]
    Kits,
    /// Remove a kit and its genotype store
    Rm(RmArgs),
    /// Genotype counts, per-chromosome counts and sex inference
    Summary(SummaryArgs),
    /// Look up genotypes by rsid or position
    Lookup(LookupArgs),
    /// Lift positions between GRCh37 and GRCh38 (UCSC chain files)
    Liftover(LiftoverArgs),
    /// Curated rsid coordinate table; `import` a dbSNP VCF for full rsid backfill
    #[command(name = "rsid-table", args_conflicts_with_subcommands = true)]
    RsidTable(RsidTableArgs),
    /// Concordance between two kits (lifting B to A's build when needed)
    Compare(CompareArgs),
    /// Export a kit as VCF, TSV or JSON (choose with --format)
    Export(ExportArgs),
    /// FASTQ -> VCF pipeline (align, sort, markdup, call, filter, normalize, import)
    #[command(subcommand)]
    Pipeline(PipelineCmd),
    /// Report external tools, cache and data locations
    Doctor,
    /// Encryption at rest: init, encrypt (migrate), rekey, unlock, lock, status
    #[command(subcommand)]
    Db(DbCmd),
    /// Audit trail of commands that read or modify personal data
    #[command(subcommand)]
    Audit(AuditCmd),
    /// Decrypt a file written with --encrypt-output or `pipeline run --seal`
    Decrypt(DecryptArgs),
    /// Configuration
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Generate shell completions
    Completions(CompletionsArgs),
    /// Generate man page(s)
    Man(ManArgs),
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Genotype file: 23andMe/AncestryDNA/MyHeritage/FTDNA .txt/.csv, or .vcf/.vcf.gz (gVCF too)
    pub file: PathBuf,
    /// Kit name (default: file stem)
    #[arg(long)]
    pub name: Option<String>,
    /// Input format (`--format` also accepts these values for import)
    #[arg(long, value_enum, default_value = "auto")]
    pub input_format: crate::parse::InputFormat,
    /// VCF sample column to import (default: first)
    #[arg(long)]
    pub sample: Option<String>,
    /// Override the detected genome build
    #[arg(long, value_name = "BUILD")]
    pub build: Option<String>,
    /// Assay type (default: array for exports, wgs for VCFs)
    #[arg(long, value_parser = ["array", "wgs", "wes", "panel"])]
    pub assay: Option<String>,
    /// Override reference-call semantics
    #[arg(long, value_parser = ["explicit", "absent-means-ref", "unknown"])]
    pub ref_calls: Option<String>,
    /// Record the kit as produced by `genome pipeline`
    #[arg(long, hide = true)]
    pub fastq_derived: bool,
    /// Replace an existing kit with the same name
    #[arg(long)]
    pub replace: bool,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Kit id or name
    pub kit: String,
}

#[derive(Debug, Args)]
pub struct SummaryArgs {
    /// Kit ids or names (default: all kits)
    pub kits: Vec<String>,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("what").required(true).multiple(true).args(["rsid", "pos"])))]
pub struct LookupArgs {
    /// Kit id or name
    pub kit: String,
    /// rsid(s) to look up (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub rsid: Vec<String>,
    /// Position(s) CHR:POS (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub pos: Vec<String>,
    /// Build of --pos coordinates (default: the kit's build); lifted to the kit's build when different
    #[arg(long, value_name = "BUILD")]
    pub build: Option<String>,
}

#[derive(Debug, Args)]
pub struct LiftoverArgs {
    /// Positions CHR:POS to lift
    pub loci: Vec<String>,
    /// Source build
    #[arg(long, default_value = "GRCh37")]
    pub from: String,
    /// Target build (default: the other build)
    #[arg(long)]
    pub to: Option<String>,
    /// Use this chain file instead of the cached UCSC file
    #[arg(long, value_name = "FILE")]
    pub chain: Option<PathBuf>,
    /// Download and verify both UCSC chain files into the cache
    #[arg(long)]
    pub fetch: bool,
}

#[derive(Debug, Args)]
pub struct RsidTableArgs {
    #[command(subcommand)]
    pub command: Option<RsidTableCmd>,
    /// Only these rsids
    #[arg(long, value_delimiter = ',')]
    pub rsid: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum RsidTableCmd {
    /// Index a dbSNP VCF (e.g. GCF_000001405.40.gz) into the cache for full rsid backfill
    Import(RsidImportArgs),
}

#[derive(Debug, Args)]
pub struct RsidImportArgs {
    /// dbSNP VCF (.vcf or .vcf.gz)
    pub vcf: PathBuf,
    /// Build of the VCF (default: from its header)
    #[arg(long)]
    pub build: Option<String>,
    /// Records per in-memory sort run
    #[arg(long, default_value_t = 50_000_000, hide = true)]
    pub chunk: usize,
}

#[derive(Debug, Args)]
pub struct CompareArgs {
    /// Kit A (its build is used for the comparison)
    pub a: String,
    /// Kit B (lifted to A's build when they differ)
    pub b: String,
    /// Cap on listed discordant sites (default: config max_discordant)
    #[arg(long)]
    pub max_discordant: Option<usize>,
    /// Restrict to a region CHR[:START-END] in A's build
    #[arg(long)]
    pub region: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Kit id or name
    pub kit: String,
    /// Restrict to a region CHR[:START-END] (repeatable)
    #[arg(long)]
    pub region: Vec<String>,
    /// Skip the first N calls (after region filtering), for paging
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Emit at most N calls (after --offset), for paging
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum PipelineCmd {
    /// Dry run: print every step and argv without running anything
    Plan(PipelineArgs),
    /// Run the pipeline (resumable: steps with up-to-date outputs are skipped)
    Run(PipelineArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PipelineArgs {
    /// FASTQ files: R1/R2 pairs, multi-lane names like S_L001_R1_001.fastq.gz are grouped
    #[arg(required = true)]
    pub fastq: Vec<PathBuf>,
    /// Output directory
    #[arg(long)]
    pub out: PathBuf,
    /// Reference: GRCh38 (no-alt analysis set, fetched into the cache) or a FASTA path
    #[arg(long)]
    pub reference: Option<String>,
    /// Restrict calling (and alignment input) to a region, e.g. chr19:44.9M-45.0M
    #[arg(long)]
    pub region: Option<String>,
    /// Use only the first N read pairs of each lane
    #[arg(long)]
    pub max_reads: Option<u64>,
    /// Threads for external tools
    #[arg(long)]
    pub threads: Option<u32>,
    /// Variant caller
    #[arg(long, value_parser = ["bcftools", "deepvariant"])]
    pub caller: Option<String>,
    /// Aligner
    #[arg(long, value_parser = ["minimap2", "bwa-mem2"])]
    pub aligner: Option<String>,
    /// Sample name (default: from the FASTQ file names)
    #[arg(long)]
    pub sample: Option<String>,
    /// Kit name for the final import (default: sample name)
    #[arg(long)]
    pub name: Option<String>,
    /// Do not import the final VCF as a kit
    #[arg(long)]
    pub no_import: bool,
    /// Re-run every step even if outputs are up to date
    #[arg(long)]
    pub force: bool,
    /// After a successful run, encrypt the final VCF with the database key (`<sample>.vcf.gz.sealed`)
    /// and shred every intermediate under --out (reads, BAMs, VCFs, logs)
    #[arg(long)]
    pub seal: bool,
}

#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// Create a new database: encrypted by default (plaintext only with --insecure-plaintext)
    Init {
        /// Encrypt the new database (the default; accepted for explicitness)
        #[arg(long)]
        encrypt: bool,
        /// Key source: auto (GENOME_KEY if set, else OS keyring, else prompt), keyring, passphrase
        #[arg(long, value_parser = ["auto", "keyring", "passphrase"])]
        kek: Option<String>,
    },
    /// Encrypt an existing plaintext database and its genotype stores in place
    Encrypt {
        /// Key source for the new key (see `db init`)
        #[arg(long, value_parser = ["auto", "keyring", "passphrase"])]
        kek: Option<String>,
    },
    /// Re-wrap the database key under a new key (new passphrase from GENOME_NEW_KEY or a prompt)
    Rekey {
        /// Key source for the new key
        #[arg(long, value_parser = ["auto", "keyring", "passphrase"])]
        kek: Option<String>,
    },
    /// Cache the passphrase-derived key in the OS keyring until `db lock`
    Unlock,
    /// Forget a cached key (`db unlock`) and remove stale temporary files
    Lock,
    /// Encryption status: cipher, key source, sealed stores, audit log
    Status,
}

#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// Show (and verify) the audit log
    Log {
        /// Only the last N entries
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Debug, Args)]
pub struct DecryptArgs {
    /// File written by --encrypt-output (passphrase: GENOME_EXPORT_KEY or prompt) or `pipeline run --seal`
    pub file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Show settings (file values, or every resolved value with --effective)
    Show {
        /// Include defaults, env and flags with their sources
        #[arg(long)]
        effective: bool,
    },
    /// Set a key in the config file
    Set { key: String, value: String },
    /// Remove a key from the config file
    Unset { key: String },
    /// Print the config file path
    Path,
    /// List known keys
    Keys,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
    Nushell,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Debug, Args)]
pub struct ManArgs {
    /// Write one page per subcommand into DIR (default: print genome.1 to stdout)
    #[arg(long)]
    pub dir: Option<PathBuf>,
}
