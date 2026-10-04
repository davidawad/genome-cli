# Personal genomic data: three kinds, one genotype model

genome-cli reads three kinds of personal genomic data. They look nothing alike
on disk, but they all describe genotypes, so they share one model: **a kit is
a set of calls, each call a genotype at a position of a stated genome build**.
What differs is how much of the genome each kind covers and what a *missing*
site means.

## 1. Genotyping-array exports (23andMe, AncestryDNA, MyHeritage, FTDNA)

| | |
|---|---|
| Files | `.txt` (23andMe, AncestryDNA) or `.csv` (MyHeritage, FTDNA), often shipped zipped (unzip first) |
| Content | ~600k pre-chosen SNPs, keyed by rsid |
| Build | almost always GRCh37 (stated in the header comments; FTDNA has no header, so GRCh37 is assumed) |
| Strand | genotype letters on the + strand of GRCh37 |
| Coverage | a **subset** of the genome: good rsid coverage of common SNPs, no indels (beyond `I`/`D` codes), no rare variants |
| Missing site | *unknown*. The chip did not probe it. Never reference. |

Per-vendor quirks the importer normalises:

- **23andMe**: `rsid chromosome position genotype`, tab-separated. `--` is a
  no-call; male X/Y/MT calls are often a single letter (hemizygous).
  Internal ids (`i3000001`) appear where there is no rsid. `II`/`DD`/`DI`
  encode indels.
- **AncestryDNA**: `rsid chromosome position allele1 allele2`. Chromosomes are
  numeric: 23 = X, 24 = Y, 25 = X pseudo-autosomal, 26 = MT. `0` is a no-call.
- **MyHeritage / FTDNA**: quoted CSV `RSID,CHROMOSOME,POSITION,RESULT`;
  MyHeritage adds `#` comments naming itself and the build.

Arrays carry **no reference allele**. Homozygous calls therefore cannot be
classified as `hom_ref` or `hom_alt` from the file alone; genome-cli resolves
them through the rsid coordinate table (bundled curated SNPs, or a dbSNP index
you import) or a reference FASTA. Unresolved ones are counted as
`hom_unknown_ref` in summaries and reported with zygosity `hom` in lookups.

`ref_calls = explicit`: every site the kit knows about is listed.

## 2. Whole-genome sequencing VCF (e.g. Nucleus; `.vcf.gz`, GRCh38)

| | |
|---|---|
| Files | `.vcf` / `.vcf.gz` (bgzip), sometimes gVCF (`.g.vcf.gz`) |
| Content | the **derived product** of sequencing: calls made by an aligner + variant caller |
| Build | usually GRCh38; detected from `##contig` lengths (chr1 = 249,250,621 → GRCh37, 248,956,422 → GRCh38) or the `##reference` header |
| IDs | the ID column is often all `.` (no rsids) |
| Contigs | besides 1–22, X, Y, MT: alt haplotypes (`chr19_KI270938v1_alt`), decoys (`hs37d5`, `chrUn_*`), unlocalized (`*_random`), HLA alleles, EBV. genome-cli groups all of these as `other_contigs` |
| Missing site | in a **variant-only** VCF, a covered site that is absent is **homozygous reference** |

Variant-only VCFs list only sites where the sample differs from the reference.
Because the assay covers (nearly) the whole genome, absence is information:
`ref_calls = absent-means-ref`. When you look up a site the file does not
list, genome-cli answers `hom_ref` with `call_source: "inferred_ref"` and says
so in `warnings`. (Caveat: absence cannot distinguish "reference" from "not
covered / failed QC"; a gVCF can.)

A **gVCF** adds explicit reference blocks (`ALT = <NON_REF>` or `<*>`, with
`INFO/END`), so every covered base is stated and absence means *not covered*:
`ref_calls = explicit`, `source_format = gvcf`. Lookups inside a block return
an observed `hom_ref`.

Exome or panel VCFs (`--assay wes|panel`) get `ref_calls = unknown`: absence
off-target says nothing.

## 3. FASTQ (raw paired-end reads)

| | |
|---|---|
| Files | `*_R1_001.fastq.gz` / `*_R2_001.fastq.gz`, often several lanes (`_L001_`, `_L002_`), tens of GB |
| Content | raw reads; **no genotypes at all** |

FASTQ must be aligned to a reference and variant-called to become kind (2).
`genome pipeline run` does exactly that with standard tools (minimap2 or
bwa-mem2, samtools, bcftools or DeepVariant) and imports the result as a kit
with `source_format = fastq-derived`. See [pipeline.md](pipeline.md).

## Why one model

- The **WGS VCF is the most important source**: it covers the genome.
- **Arrays are a subset**: the same genotypes at ~600k sites, on GRCh37.
- **FASTQ is the raw form** the VCF was derived from.

So a single genotype record (`kind: "genotypes"` in
[json-schema.md](json-schema.md)) describes a call from any of them: chrom,
pos and build; ref/alt when known; genotype letters on the + strand of the
stated build; zygosity; and `call_source` (`observed`, `inferred_ref`,
`missing`) so a consumer always knows whether a `hom_ref` was read from the
file or implied by the kit's `ref_calls` semantics. Cross-build questions
(an array on GRCh37 vs a WGS VCF on GRCh38) go through native UCSC chain-file
liftover, and kits without rsids are queried by rsid through the coordinate
table.

## Sex inference

Chromosomal sex comes from two signals:

- **X heterozygosity** outside the pseudo-autosomal regions (PAR1/PAR2 behave
  like autosomes in males and are excluded): GRCh37 X:60001-2699520 and
  X:154931044-155260560; GRCh38 X:10001-2781479 and X:155701383-156030895.
- **Y calls**: for arrays, the fraction of listed Y SNPs that were called
  (females get mostly `--`; a male kit has most Y sites called); for
  VCFs, Y variant records relative to non-PAR X variant records.

Thresholds are stated in the `sex.method` field of every summary; anything in
between is `uncertain`.
