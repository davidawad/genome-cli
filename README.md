# genome-cli

Rust CLI (`genome`) for personal genomic data: genotyping-array exports,
whole-genome VCFs and raw FASTQ reads, normalized into one genotype model.
Sibling of [biomarker-cli](https://gitlab.com/davidawad/biomarker-cli); feeds
genetics.el through the versioned [`genome/v1` JSON contract](docs/json-schema.md).

## Three kinds of data, one model

1. **Array exports** (23andMe, AncestryDNA, MyHeritage, FTDNA): ~600k
   pre-chosen SNPs keyed by rsid, usually GRCh37, + strand. A *subset* of the
   genome; an unlisted site is unknown.
2. **Whole-genome VCF** (e.g. Nucleus, `.vcf.gz`, GRCh38): the derived product
   of sequencing. Usually variant sites only, so an absent covered site is
   homozygous reference; IDs may all be `.`; extra alt/decoy/HLA/EBV contigs;
   possibly a gVCF with explicit reference blocks. **The most important source.**
3. **FASTQ** (raw paired-end reads, tens of GB): no genotypes until aligned and
   variant-called into (2) by `genome pipeline`.

Every call from any of them is one record: chrom/pos/build, ref/alt, genotype
letters on the + strand, zygosity and a `call_source` (`observed`,
`inferred_ref`, `missing`) that makes each kit's "what does absence mean"
semantics (`ref_calls`) explicit. Details: [docs/formats.md](docs/formats.md).

## Quick start

```sh
genome import genome_Jane_v5_Full.txt              # 23andMe; format auto-detected
genome import sample.hard-filtered.vcf.gz --name wgs
genome kits
genome summary wgs                                  # counts, per-chrom, sex inference
genome lookup wgs --rsid rs429358,rs7412            # APOE; no rsids in the VCF -> coordinate table
genome lookup k1 --pos 19:44908684 --build GRCh38   # lifted to the kit's GRCh37
genome compare wgs k1                               # concordance, lifting k1 to GRCh38
genome export wgs --format vcf --region chr19:44.9M-45.0M
genome liftover 19:45411941                         # GRCh37 -> GRCh38
genome pipeline plan R1.fastq.gz R2.fastq.gz --out run1 --region chr19:44.9M-45.0M --max-reads 200000
genome doctor
```

Every command accepts `--format table|json|jsonl|csv|tsv` (`-f`), `--output`,
`--columns`, `--quiet`, `--verbose`. For `import`, `--format` also names the
input format (`auto|23andme|ancestry|myheritage|ftdna|vcf`); for `export` it
selects `vcf|tsv|json`.

## Commands

| command | |
|---|---|
| `import FILE [--name] [--format ...] [--sample] [--build] [--assay] [--ref-calls] [--replace]` | auto-detect, normalize and store a kit (streaming; tested with 5.2M-record `.vcf.gz`) |
| `kits`, `rm KIT` | list / remove kits (`k1`, `k2`, ... or by name) |
| `summary [KIT...]` | no-call/het/hom/hemi counts, `by_chrom` (+ `other_contigs`), sex (PAR-excluded X heterozygosity + Y call rate), caveats |
| `lookup KIT --rsid RS... --pos CHR:POS... [--build]` | genotypes; rsid -> coordinates via the rsid table for kits without rsids; `inferred_ref` for variant-only WGS VCFs |
| `liftover CHR:POS... [--from] [--to] [--chain FILE] [--fetch]` | native UCSC chain liftover; chain files fetched on demand into the cache, sha256-verified |
| `rsid-table [--rsid]`, `rsid-table import DBSNP_VCF [--build]` | bundled curated table; index a dbSNP VCF for full rsid backfill |
| `compare A B [--max-discordant N] [--region]` | concordance on overlapping sites, B lifted to A's build |
| `export KIT --format vcf\|tsv\|json [--region ...]` | |
| `pipeline plan\|run FASTQ... --out DIR [--reference GRCh38\|FASTA] [--region] [--max-reads] [--threads] [--caller bcftools\|deepvariant] [--aligner minimap2\|bwa-mem2]` | FASTQ -> VCF -> kit; see [docs/pipeline.md](docs/pipeline.md) |
| `doctor` | external tools (with `brew install` hints), cached chains/reference/dbSNP |
| `config show [--effective] \| set \| unset \| path \| keys`, `completions SHELL`, `man [--dir]` | |

### The rsid coordinate table

`data/rsid_table.tsv` (compiled in) maps curated SNPs to GRCh37 and GRCh38
chrom/pos/ref/alt: APOE rs429358 rs7412, MTHFR rs1801133 rs1801131, F5
rs6025, HFE rs1800562, LCT rs4988235, CYP2C19 rs4244285. Every row was checked
against NCBI dbSNP and Ensembl (both builds) and cites them. For any other
rsid on a kit without rsids, index a dbSNP VCF once:

```sh
genome rsid-table import GCF_000001405.40.gz      # GRCh38 dbSNP; GCF_000001405.25 for GRCh37
```

The index (sorted by rsid and by position, 12 bytes/record) lives in
`<cache_dir>/dbsnp/` and is built with bounded memory (external merge sort).

## Configuration

Layers, lowest to highest: built-in defaults < `~/.config/genome-cli/config.toml`
(or `$XDG_CONFIG_HOME`, `--config`, `GENOME_CONFIG`) < `GENOME_*` environment
variables < flags. `genome config show --effective` prints every value with
its source; `genome config keys` lists keys and env names.

| key | env | default |
|---|---|---|
| `data_dir` | `GENOME_DATA_DIR` | `~/.local/share/genome-cli` |
| `db_path` | `GENOME_DB` | `<data_dir>/genome.db` |
| `cache_dir` | `GENOME_CACHE_DIR` | `~/.cache/genome-cli` |
| `format`, `color`, `precision`, `csv_delimiter`, `csv_header`, `null` | `GENOME_FORMAT`, ... | `table`, `auto`, 3, `,`, true, empty |
| `max_discordant` | `GENOME_MAX_DISCORDANT` | 50 |
| `threads`, `aligner`, `caller`, `container`, `reference` | `GENOME_THREADS`, ... | 4, `minimap2`, `bcftools`, `auto`, `GRCh38` |
| `reference_grch37`, `reference_grch38` | | optional FASTA (+`.fai`) used to fill reference alleles for `inferred_ref` and array sites |
| `ucsc_url`, `reference_url`, `offline` | `GENOME_OFFLINE` | UCSC goldenPath, NCBI no-alt set, false |

## Storage

Kit metadata (one row per kit, including the precomputed summary) is in a
FrankenSQLite (fsqlite) database, wired exactly like biomarker-cli. The
genotype table is **not**. Measured with `examples/fsqlite_bench.rs`
(`cargo run --release --example fsqlite_bench -- N`; release build, one
transaction, x86_64, fsqlite 0.4.9), fsqlite inserted 7,392 genotype rows/s
at 50k rows and 5,313 rows/s at 200k rows (point lookups ~0.26 ms). Insert
throughput falls as the table grows, so a 5M-record WGS VCF would take well over
16 minutes. That is not acceptable for an import. Instead each kit gets a write-once, sorted binary store in
`<data_dir>/kits/<id>/` (`src/gtstore.rs`):

- `sites.bin`: 32-byte records sorted by (contig, pos), binary-searched with
  positioned reads (lookups never load the kit);
- `heap.bin`: rsid/ref/alt/genotype/filter/GT strings;
- `rsid.idx`: sorted (rs number, record) pairs;
- `contigs.json`.

Same machine, a synthetic 5.22M-record `.vcf.gz` (19 MB): `genome import`
takes **10.4 s** with 194 MB peak RSS. The resulting store is 259 MB.
`lookup --pos` takes ~40 ms, and `summary` is instant because it is computed
at import.

## Exit codes

0 ok, 1 error, 2 usage, 3 not found, 4 invalid data, 5 database, 6 io,
7 config, 8 network, 9 external tool. With `--format json` errors are printed
on stdout as `{"schema":"genome/v1","ok":false,"error":{"code":...,"message":...}}`.

## Building

fsqlite 0.4.x uses `#![feature(core_intrinsics)]` on x86_64, so x86_64 needs
a **nightly** toolchain; `rust-toolchain.toml` selects it automatically under
rustup. On aarch64 (Apple Silicon) it builds on stable (e.g. Homebrew's
cargo, which ignores the toolchain file).

```sh
cargo build --release          # target/release/genome
just test-gate                 # fmt --check, clippy -D warnings, tests
genome man --dir /usr/local/share/man/man1
genome completions zsh > ~/.zfunc/_genome
```

## Tests

Synthetic fixtures only (`tests/fixtures`, regenerated by `gen_fixtures.py`):
tiny exports in each array format (male and female 23andMe), GRCh38 and
GRCh37 VCFs with and without rsids and with alt/decoy/HLA/EBV contigs, a
gVCF, and tiny chain files. `tests/pipeline_e2e.rs` simulates read pairs from
a synthetic reference with planted SNPs and runs the real
minimap2/samtools/bcftools pipeline when they are installed (it prints a skip
message otherwise). No real person's genome is fetched or used.

## License

MIT
