# genome/v1 JSON contract

Shared by genome-cli and genetics.el. Every `genome <cmd> --format json`
prints exactly one envelope on stdout:

```json
{"schema":"genome/v1","kind":"<kind>","generated_at":"<RFC3339>","count":N,"data":[...],"warnings":[...]}
```

- `count` is always `data.length`.
- `warnings` is always present (possibly empty). In table/csv/tsv/jsonl modes
  warnings go to stderr as `genome: warning: ...`.
- `--format jsonl` prints one `data` element per line, without the envelope.
- Fields not listed below may be added in future `genome/v1` releases;
  consumers must ignore unknown fields. Removing or renaming a field requires
  `genome/v2`.

## Errors

```json
{"schema":"genome/v1","ok":false,"error":{"code":"not_found","exit_code":3,"message":"no kit 'x' (see `genome kits`)"}}
```

Printed on stdout in json/jsonl modes (a human-readable line always goes to
stderr), with a nonzero exit status:

| exit | code | meaning |
|---:|---|---|
| 0 | | success |
| 1 | `general` | unspecified failure |
| 2 | `usage` | bad command line or argument value |
| 3 | `not_found` | kit, file, sample, cached file not found |
| 4 | `invalid` | unparseable/unsupported input, duplicate kit name |
| 5 | `database` | fsqlite error |
| 6 | `io` | filesystem error |
| 7 | `config` | config file/value problem |
| 8 | `network` | download or checksum failure |
| 9 | `tool` | external tool missing or failed (pipeline) |

## kind `kits` (`genome kits`, `genome import`, `genome rm`)

```json
{"id":"k1","name":"...","source_format":"23andme|ancestry|myheritage|ftdna|vcf|gvcf|fastq-derived",
 "assay":"array|wgs|wes|panel","build":"GRCh37|GRCh38|unknown","build_evidence":"header|contig-lengths|assumed",
 "sample":"...","records":N,"has_rsids":bool,"ref_calls":"explicit|absent-means-ref|unknown",
 "chip":"v5|unknown|null","imported_at":"...","source_path":"..."}
```

- `has_rsids`: at least half of the records carry an `rs` id.
- `chip`: arrays report `"unknown"` (vendors do not state the chip version in
  exports); VCF kits report `null`.
- `build_evidence`: `assumed` also covers `--build` overrides (a warning says so).

## kind `summary` (`genome summary`)

```json
{"kit":"k1","records":N,"no_calls":N,"het":N,"hom_alt":N,"hom_ref":N,"hemizygous":N,
 "by_chrom":{"1":N,...,"22":N,"X":N,"Y":N,"MT":N,"other_contigs":N},
 "sex":{"call":"male|female|uncertain","x_het_rate":f,"y_call_rate":f,"method":"...","x_sites":N,"y_sites":N},
 "caveats":["..."],
 "hom_unknown_ref":N,"ref_blocks":N}
```

- `other_contigs` groups alt, decoy, HLA, `_random`, `chrUn`, EBV and any other
  non-primary contig.
- `hom_unknown_ref` (extension): homozygous array calls whose reference allele
  is unknown. `records = no_calls + het + hom_alt + hom_ref + hom_unknown_ref + hemizygous`.
- `ref_blocks` (extension): gVCF reference-block records (also counted in `hom_ref`).
- `x_het_rate` excludes PAR1/PAR2. For arrays `y_call_rate` = called / listed Y
  SNPs; for VCFs it is called Y records / called non-PAR X records. `method`
  states the thresholds used.

## kind `genotypes` (`genome lookup`, `genome export --format json|tsv`)

```json
{"kit":"k1","rsid":"rs429358","chrom":"19","pos":44908684,"build":"GRCh38","ref":"T","alt":["C"],
 "genotype":"CT","zygosity":"het|hom_ref|hom_alt|hemi|no_call","call_source":"observed|inferred_ref|missing",
 "filter":"PASS","quality":50.0,"depth":30,"lifted_from":null}
```

- Genotype letters are on the + strand of `build`. SNVs are concatenated
  (`"CT"`); when any allele is longer than one base they are `/`-joined
  (`"AT/A"`). Array indel codes (`"DI"`) pass through.
- `chrom` is normalised: no `chr` prefix, `MT` for mitochondria.
- `call_source: "inferred_ref"`: the site is absent from a variant-only WGS VCF
  (`ref_calls = absent-means-ref`) and is reported as `hom_ref` because the
  assay covers the genome; a warning says so. `genotype` is the reference
  base twice when it is known (curated table, dbSNP index or a configured
  reference FASTA), otherwise `null`.
- `call_source: "missing"`: not in the kit and not inferable (array site not
  on the chip, gVCF region without coverage, unknown rsid). `zygosity` is
  `no_call`, `genotype` is `null`.
- `zygosity: "hom"` (extension) appears only for array calls whose reference
  allele could not be resolved.
- `lifted_from`: set when the query coordinate was given in another build
  (`lookup --pos ... --build GRCh37` on a GRCh38 kit, or an rsid resolved in
  the other build): `{"build":"GRCh37","pos":45411941}`. `chrom`/`pos`/`build`
  are always in the kit's build.

## kind `compare` (`genome compare A B`)

```json
{"a":"k1","b":"k2","build":"GRCh38","overlap":N,"concordant":N,"discordant":N,"concordance":f,
 "discordant_sites":[{"chrom":"1","pos":11794419,"rsid":"rs1801131","a":{genotype},"b":{genotype}}],
 "inferred_ref_sites":N,"skipped_sites":N,"lifted":bool}
```

- B is lifted to A's build when they differ (`lifted: true`).
- Sites absent from a variant-only WGS kit are compared as inferred `hom_ref`
  (`inferred_ref_sites`); absent sites of explicit kits (arrays, gVCF) are not
  part of the overlap.
- Only SNV genotypes are compared (unordered alleles; hemizygous `A` == `AA`).
  `skipped_sites` counts overlapping indels and inferred sites whose reference
  base is unknown.
- `discordant_sites` is capped (`--max-discordant`, config `max_discordant`, default 50).

## kinds `pipeline-plan` / `pipeline-run` (`genome pipeline plan|run`)

```json
{"step":"fetch-reference|index|align|sort|markdup|call|filter|normalize|import","tool":"minimap2",
 "argv":["minimap2","-ax","sr",...],"inputs":["..."],"outputs":["..."],
 "status":"planned|skipped-cached|done|failed","seconds":f,"stdout_to_next":bool,"command":"minimap2 -ax sr ... |"}
```

- `stdout_to_next` (extension): the step's stdout is piped into the next step
  (e.g. `minimap2 | samtools fixmate | samtools sort`); a piped group shares
  one `seconds` value.
- Steps run by genome itself (`fetch-reference`, `--max-reads` subsampling,
  `import`) have `tool: "genome"` and an informational `argv`.

## Other kinds

`liftover`, `liftover-chains`, `rsid-table`, `rsid-table-import`, `doctor`,
`config`, `config_keys`, `config_path` use the same envelope; their record
fields are self-describing.
