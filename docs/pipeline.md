# FASTQ -> VCF pipeline

`genome pipeline plan|run R1 R2 [R1 R2 ...] --out DIR` turns raw paired-end
reads into a VCF with standard tools found on `PATH`, then imports it as a kit
(`source_format: fastq-derived`).

```sh
genome doctor                       # which tools are present, install hints
genome pipeline plan reads/*.fastq.gz --out run1 --region chr19:44.9M-45.0M --max-reads 200000
genome pipeline run  reads/*.fastq.gz --out run1 --region chr19:44.9M-45.0M --max-reads 200000
```

## Steps

| step | tool | what |
|---|---|---|
| fetch-reference | genome | download GRCh38 no-alt analysis set (`GCA_000001405.15_GRCh38_no_alt_analysis_set.fna.gz`, md5-verified against NCBI's `md5checksums.txt`) into `<cache_dir>/reference/` and decompress. Skipped for `--reference /path/to.fa`. |
| index | samtools | `samtools faidx` |
| index | minimap2 / bwa-mem2 | `minimap2 -x sr -d ref.sr.mmi` or `bwa-mem2 index` (once; cached) |
| align | genome | with `--max-reads N`: first N read pairs of each lane |
| align | minimap2 / bwa-mem2 | `-ax sr` / `mem`, read group per lane, piped into |
| markdup | samtools | `fixmate -m` (mate tags for markdup), piped into |
| sort | samtools | `sort` -> per-lane BAM |
| sort | samtools | `merge` lanes (multi-lane only) |
| markdup | samtools | `markdup` |
| index | samtools | `index` |
| call | bcftools | `mpileup -a AD,DP [-r REGION] | call -mv` (or DeepVariant in docker/podman with `--caller deepvariant`) |
| filter | bcftools | soft-filter `QUAL<20 || DP<5` as `LowQual` (DeepVariant: keep PASS) |
| normalize | bcftools | `norm -f ref -m -any`, then `index -t` |
| import | genome | `genome import <out>/<sample>.vcf.gz --name <sample> --replace` |

## Lanes

Files named like `SAMPLE_S1_L001_R1_001.fastq.gz` / `..._R2_001.fastq.gz` are
paired by their R1/R2 token and grouped by lane (`L001`, `L002`, ...). Each
lane is aligned with its own read group (`ID:SAMPLE.L001`, `SM:SAMPLE`) and
lanes are merged before duplicate marking. `x_1.fq.gz`/`x_2.fq.gz` and
`x_R1.fastq`/`x_R2.fastq` also pair.

## Resumability

Each step (or piped group of steps) declares inputs and outputs. `run` skips a
group whose outputs all exist and are newer than its inputs
(`status: skipped-cached`); once a group re-runs, everything downstream
re-runs. Failed groups delete their partial outputs. `--force` re-runs all.
Logs: `<out>/logs/NN-step-tool.log`.

`plan` is a pure dry run: it prints every argv and which steps would be
skipped, and creates nothing.

## Small runs on a laptop

`--max-reads N` truncates every lane to its first N read pairs and
`--region` restricts calling (`bcftools mpileup -r`, DeepVariant
`--regions`). The reference index is built once and cached (minimap2 needs
~11 GB RAM and a few minutes for GRCh38; for even smaller tests pass
`--reference` with a small FASTA).

## Tests

`tests/pipeline_e2e.rs` simulates a few thousand read pairs (Rust, no wgsim
needed) from a synthetic 20 kb reference with known heterozygous and
homozygous SNPs, runs the real pipeline when minimap2, samtools and bcftools
are on PATH, and checks the imported kit reports the planted genotypes. It
prints a skip message otherwise.
