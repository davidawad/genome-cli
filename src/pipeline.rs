//! FASTQ -> VCF pipeline orchestrating external tools found on PATH.
//!
//! Steps (genome/v1 `pipeline-plan` / `pipeline-run`):
//! fetch-reference -> index -> per lane: align | fixmate (markdup prep) | sort
//! -> merge lanes (sort) -> markdup -> index -> call -> filter -> normalize -> import.
//! A step whose stdout feeds the next step runs as one OS pipeline. A step
//! group is skipped when all its outputs exist and are newer than its inputs.

use std::fs::File;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::time::{Instant, SystemTime};

use serde_json::{json, Value};

use crate::cli::{ImportArgs, PipelineArgs, PipelineCmd};
use crate::context::Ctx;
use crate::error::{AppError, Result};
use crate::model::Region;
use crate::output::{Record, Report};

pub const GRCH38_NO_ALT_URL: &str = "https://ftp.ncbi.nlm.nih.gov/genomes/all/GCA/000/001/405/GCA_000001405.15_GRCh38/\
seqs_for_alignment_pipelines.ucsc_ids/GCA_000001405.15_GRCh38_no_alt_analysis_set.fna.gz";
/// From NCBI's md5checksums.txt in the same directory (checked 2026-10-03).
pub const GRCH38_NO_ALT_MD5: &str = "a08035b6a6e31780e96a34008ff21bd6";
pub const DEEPVARIANT_IMAGE: &str = "google/deepvariant:1.6.1";

pub fn cached_reference_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("reference").join("GCA_000001405.15_GRCh38_no_alt_analysis_set.fna")
}

#[derive(Debug, Clone)]
pub enum Builtin {
    FetchReference { url: String, gz: PathBuf, dest: PathBuf },
    Subsample { r1: PathBuf, r2: PathBuf, n: u64, out1: PathBuf, out2: PathBuf },
    Import { vcf: PathBuf, name: String, marker: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub step: &'static str,
    pub tool: String,
    pub argv: Vec<String>,
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    /// stdout is piped into the next step.
    pub pipe_next: bool,
    pub builtin: Option<Builtin>,
    pub status: &'static str,
    pub seconds: Option<f64>,
}

impl Step {
    fn exec(step: &'static str, argv: Vec<String>, inputs: Vec<PathBuf>, outputs: Vec<PathBuf>) -> Self {
        Self {
            step,
            tool: argv.first().cloned().unwrap_or_default(),
            argv,
            inputs,
            outputs,
            pipe_next: false,
            builtin: None,
            status: "planned",
            seconds: None,
        }
    }
    fn piped(mut self) -> Self {
        self.pipe_next = true;
        self
    }
    fn record(&self) -> Record {
        let paths = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let mut r = Record::new();
        r.insert("step".into(), json!(self.step));
        r.insert("tool".into(), json!(self.tool));
        r.insert("argv".into(), json!(self.argv));
        r.insert("inputs".into(), json!(paths(&self.inputs)));
        r.insert("outputs".into(), json!(paths(&self.outputs)));
        r.insert("status".into(), json!(self.status));
        r.insert("seconds".into(), json!(self.seconds.map(|s| (s * 1000.0).round() / 1000.0)));
        r.insert("stdout_to_next".into(), json!(self.pipe_next));
        r.insert("command".into(), Value::String(shell_join(&self.argv) + if self.pipe_next { " |" } else { "" }));
        r
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+%".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A paired-end lane.
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub id: String,
    pub sample: String,
    pub r1: PathBuf,
    pub r2: PathBuf,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The R2 name for an R1 name, if it follows a known convention.
fn mate_name(name: &str) -> Option<String> {
    for (a, b) in [("_R1_", "_R2_"), ("_R1.", "_R2."), ("_1.", "_2."), (".R1.", ".R2."), ("_r1.", "_r2.")] {
        if let Some(i) = name.rfind(a) {
            return Some(format!("{}{}{}", &name[..i], b, &name[i + a.len()..]));
        }
    }
    None
}

fn sample_and_lane(name: &str) -> (String, Option<String>) {
    let stem =
        ["_R1_", "_R1.", "_1.", ".R1.", "_r1."].iter().find_map(|m| name.rfind(m).map(|i| &name[..i])).unwrap_or(name);
    let (stem, lane) = match stem.rfind("_L") {
        Some(i) if stem[i + 2..].len() == 3 && stem[i + 2..].chars().all(|c| c.is_ascii_digit()) => {
            (&stem[..i], Some(stem[i + 1..].to_string()))
        }
        _ => (stem, None),
    };
    // Illumina sample numbers: NAME_S1_L001 -> NAME
    let stem = match stem.rfind("_S") {
        Some(i) if !stem[i + 2..].is_empty() && stem[i + 2..].chars().all(|c| c.is_ascii_digit()) => &stem[..i],
        _ => stem,
    };
    (stem.to_string(), lane)
}

/// Pair R1/R2 files into lanes.
pub fn pair_lanes(files: &[PathBuf]) -> Result<Vec<Lane>> {
    let names: Vec<String> = files.iter().map(|p| file_name(p)).collect();
    let mut used = vec![false; files.len()];
    let mut lanes = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if used[i] {
            continue;
        }
        let Some(m) = mate_name(n) else { continue };
        if let Some(j) = names.iter().position(|x| *x == m) {
            used[i] = true;
            used[j] = true;
            let (sample, lane) = sample_and_lane(n);
            let id = lane.unwrap_or_else(|| format!("L{:03}", lanes.len() + 1));
            lanes.push(Lane { id, sample, r1: files[i].clone(), r2: files[j].clone() });
        }
    }
    if let Some(i) = used.iter().position(|u| !u) {
        return Err(AppError::usage(format!(
            "cannot pair {} with an R2 file (expected names like S_L001_R1_001.fastq.gz / S_L001_R2_001.fastq.gz)",
            files[i].display()
        )));
    }
    if lanes.is_empty() {
        return Err(AppError::usage("no FASTQ pairs given"));
    }
    lanes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(lanes)
}

pub struct Settings {
    pub out: PathBuf,
    pub reference: PathBuf,
    pub fetch_url: Option<String>,
    pub index_dir: PathBuf,
    pub region: Option<Region>,
    pub max_reads: Option<u64>,
    pub threads: u32,
    pub caller: String,
    pub aligner: String,
    pub container: String,
    pub sample: String,
    pub name: String,
    pub import: bool,
}

/// Build the full step list.
pub fn plan(st: &Settings, lanes: &[Lane]) -> Vec<Step> {
    let t = st.threads.to_string();
    let reference = &st.reference;
    let fai = PathBuf::from(format!("{}.fai", s(reference)));
    let out = &st.out;
    let mut steps = Vec::new();
    if let Some(url) = &st.fetch_url {
        let gz = PathBuf::from(format!("{}.gz", s(reference)));
        steps.push(Step {
            step: "fetch-reference",
            tool: "genome".into(),
            argv: vec![
                "genome-builtin".into(),
                "fetch".into(),
                url.clone(),
                "--md5".into(),
                GRCH38_NO_ALT_MD5.into(),
                s(reference),
            ],
            inputs: Vec::new(),
            outputs: vec![reference.clone()],
            pipe_next: false,
            builtin: Some(Builtin::FetchReference { url: url.clone(), gz, dest: reference.clone() }),
            status: "planned",
            seconds: None,
        });
    }
    steps.push(Step::exec(
        "index",
        vec!["samtools".into(), "faidx".into(), s(reference)],
        vec![reference.clone()],
        vec![fai.clone()],
    ));
    let ref_name = file_name(reference);
    let aligner_index: PathBuf;
    if st.aligner == "bwa-mem2" {
        let prefix = st.index_dir.join(&ref_name);
        aligner_index = prefix.clone();
        let outs = ["0123", "amb", "ann", "bwt.2bit.64", "pac"]
            .iter()
            .map(|e| PathBuf::from(format!("{}.{e}", s(&prefix))))
            .collect();
        steps.push(Step::exec(
            "index",
            vec!["bwa-mem2".into(), "index".into(), "-p".into(), s(&prefix), s(reference)],
            vec![reference.clone()],
            outs,
        ));
    } else {
        aligner_index = st.index_dir.join(format!("{ref_name}.sr.mmi"));
        steps.push(Step::exec(
            "index",
            vec![
                "minimap2".into(),
                "-x".into(),
                "sr".into(),
                "-t".into(),
                t.clone(),
                "-d".into(),
                s(&aligner_index),
                s(reference),
            ],
            vec![reference.clone()],
            vec![aligner_index.clone()],
        ));
    }
    let aln_index_input = if st.aligner == "bwa-mem2" {
        PathBuf::from(format!("{}.bwt.2bit.64", s(&aligner_index)))
    } else {
        aligner_index.clone()
    };
    let mut lane_bams = Vec::new();
    for lane in lanes {
        let (mut r1, mut r2) = (lane.r1.clone(), lane.r2.clone());
        if let Some(n) = st.max_reads {
            let (o1, o2) = (
                out.join("reads").join(format!("{}_{}_R1.fastq", lane.sample, lane.id)),
                out.join("reads").join(format!("{}_{}_R2.fastq", lane.sample, lane.id)),
            );
            steps.push(Step {
                step: "align",
                tool: "genome".into(),
                argv: vec!["genome-builtin".into(), "head-reads".into(), n.to_string(), s(&r1), s(&r2), s(&o1), s(&o2)],
                inputs: vec![r1.clone(), r2.clone()],
                outputs: vec![o1.clone(), o2.clone()],
                pipe_next: false,
                builtin: Some(Builtin::Subsample {
                    r1: r1.clone(),
                    r2: r2.clone(),
                    n,
                    out1: o1.clone(),
                    out2: o2.clone(),
                }),
                status: "planned",
                seconds: None,
            });
            r1 = o1;
            r2 = o2;
        }
        let rg = format!("@RG\\tID:{}.{}\\tSM:{}\\tLB:{}\\tPL:ILLUMINA", st.sample, lane.id, st.sample, st.sample);
        let align = if st.aligner == "bwa-mem2" {
            vec![
                "bwa-mem2".into(),
                "mem".into(),
                "-t".into(),
                t.clone(),
                "-R".into(),
                rg,
                s(&aligner_index),
                s(&r1),
                s(&r2),
            ]
        } else {
            vec![
                "minimap2".into(),
                "-ax".into(),
                "sr".into(),
                "-t".into(),
                t.clone(),
                "-R".into(),
                rg,
                s(&aligner_index),
                s(&r1),
                s(&r2),
            ]
        };
        let bam = out.join("bam").join(format!("{}.{}.sorted.bam", st.sample, lane.id));
        let tmp = out.join("bam").join(format!("{}.{}.tmp", st.sample, lane.id));
        steps.push(
            Step::exec("align", align, vec![aln_index_input.clone(), r1.clone(), r2.clone()], Vec::new()).piped(),
        );
        steps.push(
            Step::exec(
                "markdup",
                vec!["samtools".into(), "fixmate".into(), "-m".into(), "-u".into(), "-".into(), "-".into()],
                Vec::new(),
                Vec::new(),
            )
            .piped(),
        );
        steps.push(Step::exec(
            "sort",
            vec![
                "samtools".into(),
                "sort".into(),
                "-@".into(),
                t.clone(),
                "-T".into(),
                s(&tmp),
                "-o".into(),
                s(&bam),
                "-".into(),
            ],
            Vec::new(),
            vec![bam.clone()],
        ));
        lane_bams.push(bam);
    }
    let merged = if lane_bams.len() > 1 {
        let m = out.join("bam").join(format!("{}.merged.bam", st.sample));
        let mut argv = vec!["samtools".into(), "merge".into(), "-f".into(), "-@".into(), t.clone(), "-o".into(), s(&m)];
        argv.extend(lane_bams.iter().map(|b| s(b)));
        steps.push(Step::exec("sort", argv, lane_bams.clone(), vec![m.clone()]));
        m
    } else {
        lane_bams[0].clone()
    };
    let dedup = out.join("bam").join(format!("{}.markdup.bam", st.sample));
    let bai = PathBuf::from(format!("{}.bai", s(&dedup)));
    steps.push(Step::exec(
        "markdup",
        vec!["samtools".into(), "markdup".into(), "-@".into(), t.clone(), s(&merged), s(&dedup)],
        vec![merged],
        vec![dedup.clone()],
    ));
    steps.push(Step::exec(
        "index",
        vec!["samtools".into(), "index".into(), s(&dedup)],
        vec![dedup.clone()],
        vec![bai.clone()],
    ));
    let calls = out.join("vcf").join(format!("{}.calls.vcf.gz", st.sample));
    let region = st.region.as_ref().map(|r| r.to_tool_string(&r.raw_chrom));
    if st.caller == "deepvariant" {
        let refdir = reference.parent().unwrap_or(Path::new("/")).to_path_buf();
        let mut argv = vec![
            st.container.clone(),
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{}:/ref", s(&refdir)),
            "-v".into(),
            format!("{}:/out", s(out)),
            DEEPVARIANT_IMAGE.into(),
            "/opt/deepvariant/bin/run_deepvariant".into(),
            "--model_type=WGS".into(),
            format!("--ref=/ref/{ref_name}"),
            format!("--reads=/out/bam/{}", file_name(&dedup)),
            format!("--output_vcf=/out/vcf/{}", file_name(&calls)),
            format!("--num_shards={t}"),
        ];
        if let Some(r) = &region {
            argv.push(format!("--regions={r}"));
        }
        steps.push(Step::exec("call", argv, vec![dedup.clone(), bai.clone(), fai.clone()], vec![calls.clone()]));
    } else {
        let mut mp = vec![
            "bcftools".into(),
            "mpileup".into(),
            "--threads".into(),
            t.clone(),
            "-f".into(),
            s(reference),
            "-a".into(),
            "FORMAT/AD,FORMAT/DP".into(),
            "-q".into(),
            "20".into(),
            "-Q".into(),
            "20".into(),
        ];
        if let Some(r) = &region {
            mp.extend(["-r".into(), r.clone()]);
        }
        mp.extend(["-Ou".into(), s(&dedup)]);
        steps.push(Step::exec("call", mp, vec![dedup.clone(), bai.clone(), fai.clone()], Vec::new()).piped());
        steps.push(Step::exec(
            "call",
            vec![
                "bcftools".into(),
                "call".into(),
                "--threads".into(),
                t.clone(),
                "-m".into(),
                "-v".into(),
                "-Oz".into(),
                "-o".into(),
                s(&calls),
                "-".into(),
            ],
            Vec::new(),
            vec![calls.clone()],
        ));
    }
    let filtered = out.join("vcf").join(format!("{}.filtered.vcf.gz", st.sample));
    let filter = if st.caller == "deepvariant" {
        vec![
            "bcftools".into(),
            "view".into(),
            "-f".into(),
            "PASS".into(),
            "-Oz".into(),
            "-o".into(),
            s(&filtered),
            s(&calls),
        ]
    } else {
        vec![
            "bcftools".into(),
            "filter".into(),
            "-s".into(),
            "LowQual".into(),
            "-e".into(),
            "QUAL<20 || INFO/DP<5".into(),
            "-Oz".into(),
            "-o".into(),
            s(&filtered),
            s(&calls),
        ]
    };
    steps.push(Step::exec("filter", filter, vec![calls], vec![filtered.clone()]));
    let fin = out.join(format!("{}.vcf.gz", st.sample));
    steps.push(Step::exec(
        "normalize",
        vec![
            "bcftools".into(),
            "norm".into(),
            "-f".into(),
            s(reference),
            "-m".into(),
            "-any".into(),
            "-Oz".into(),
            "-o".into(),
            s(&fin),
            s(&filtered),
        ],
        vec![filtered, fai],
        vec![fin.clone()],
    ));
    let tbi = PathBuf::from(format!("{}.tbi", s(&fin)));
    steps.push(Step::exec(
        "normalize",
        vec!["bcftools".into(), "index".into(), "-f".into(), "-t".into(), s(&fin)],
        vec![fin.clone()],
        vec![tbi],
    ));
    if st.import {
        let marker = out.join("import.json");
        steps.push(Step {
            step: "import",
            tool: "genome".into(),
            argv: vec!["genome".into(), "import".into(), s(&fin), "--name".into(), st.name.clone(), "--replace".into()],
            inputs: vec![fin.clone()],
            outputs: vec![marker.clone()],
            pipe_next: false,
            builtin: Some(Builtin::Import { vcf: fin, name: st.name.clone(), marker }),
            status: "planned",
            seconds: None,
        });
    }
    steps
}

/// Groups of step indices connected by pipes.
fn groups(steps: &[Step]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, st) in steps.iter().enumerate() {
        if !st.pipe_next {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < steps.len() {
        out.push(start..steps.len());
    }
    out
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// All outputs exist and none is older than any input.
fn up_to_date(inputs: &[PathBuf], outputs: &[PathBuf]) -> bool {
    if outputs.is_empty() {
        return false;
    }
    let Some(oldest_out) =
        outputs.iter().map(|p| mtime(p)).collect::<Option<Vec<_>>>().and_then(|v| v.into_iter().min())
    else {
        return false;
    };
    inputs.iter().filter_map(|p| mtime(p)).all(|t| t <= oldest_out)
}

fn group_io(steps: &[Step], g: &std::ops::Range<usize>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let inputs = steps[g.clone()].iter().flat_map(|s| s.inputs.clone()).collect();
    let outputs = steps[g.clone()].iter().flat_map(|s| s.outputs.clone()).collect();
    (inputs, outputs)
}

/// Mark groups whose outputs are fresh as skipped-cached (pure: only stats files).
pub fn mark_cached(steps: &mut [Step], force: bool) {
    let mut upstream_reran = false;
    for g in groups(steps) {
        let (i, o) = group_io(steps, &g);
        let cached = !force && !upstream_reran && up_to_date(&i, &o);
        if !cached {
            upstream_reran = true;
        }
        for st in &mut steps[g] {
            st.status = if cached { "skipped-cached" } else { "planned" };
        }
    }
}

pub fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(tool)).find(|p| p.is_file())
}

pub fn brew_hint(tool: &str) -> &'static str {
    match tool {
        "minimap2" => "brew install minimap2",
        "bwa-mem2" => "brew install bwa-mem2",
        "samtools" => "brew install samtools",
        "bcftools" => "brew install bcftools",
        "bgzip" | "tabix" => "brew install htslib",
        "docker" => "brew install --cask docker",
        "podman" => "brew install podman",
        "wgsim" => "conda install -c bioconda wgsim (optional; tests simulate reads natively)",
        _ => "see docs/pipeline.md",
    }
}

fn head_reads(r1: &Path, r2: &Path, n: u64, o1: &Path, o2: &Path) -> Result<()> {
    for (src, dst) in [(r1, o1), (r2, o2)] {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        let reader = crate::parse::open_text(src)?;
        let mut w = BufWriter::new(File::create(dst)?);
        for line in reader.lines().take((n * 4) as usize) {
            writeln!(w, "{}", line?)?;
        }
        w.flush()?;
    }
    Ok(())
}

fn fetch_reference(url: &str, gz: &Path, dest: &Path, ctx: &Ctx) -> Result<()> {
    if ctx.offline() {
        return Err(AppError::not_found(format!("reference {} is not cached and offline mode is on", dest.display())));
    }
    if !gz.exists() {
        ctx.info(&format!("downloading {url} (~900 MB)"));
        crate::fetch::download(url, gz, Some(crate::fetch::Checksum::Md5(GRCH38_NO_ALT_MD5)))?;
    }
    ctx.info(&format!("decompressing {}", gz.display()));
    let tmp = dest.with_extension("fna.part");
    let mut r = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(File::open(gz)?));
    let mut w = BufWriter::new(File::create(&tmp)?);
    std::io::copy(&mut r, &mut w)?;
    w.flush()?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Run one group of piped external commands.
fn run_group(steps: &[Step], logs: &Path, idx: usize) -> Result<()> {
    let mut children = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;
    for (k, st) in steps.iter().enumerate() {
        for o in &st.outputs {
            if let Some(p) = o.parent() {
                std::fs::create_dir_all(p)?;
            }
        }
        let log = logs.join(format!("{:02}-{}-{}.log", idx + k, st.step, st.tool));
        let mut cmd = Proc::new(&st.argv[0]);
        cmd.args(&st.argv[1..]).stderr(File::create(&log)?);
        cmd.stdin(match prev_stdout.take() {
            Some(o) => Stdio::from(o),
            None => Stdio::null(),
        });
        cmd.stdout(if st.pipe_next {
            Stdio::piped()
        } else {
            Stdio::from(File::create(log.with_extension("stdout"))?)
        });
        let mut child = cmd.spawn().map_err(|e| {
            AppError::tool(format!("cannot run {}: {e} (install: {})", st.argv[0], brew_hint(&st.tool)))
        })?;
        prev_stdout = child.stdout.take();
        children.push((st, child, log));
    }
    let mut failure = None;
    for (st, mut child, log) in children {
        let status = child.wait()?;
        if !status.success() && failure.is_none() {
            let tail = std::fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = tail.lines().rev().take(5).collect();
            failure = Some(AppError::tool(format!(
                "{} failed ({status}); log {}:\n{}",
                shell_join(&st.argv),
                log.display(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )));
        }
    }
    match failure {
        Some(e) => {
            for st in steps {
                for o in &st.outputs {
                    let _ = std::fs::remove_file(o);
                }
            }
            Err(e)
        }
        None => Ok(()),
    }
}

fn settings(ctx: &Ctx, a: &PipelineArgs, lanes: &[Lane]) -> Result<Settings> {
    let reference = a.reference.clone().unwrap_or_else(|| ctx.get("reference").to_string());
    let (reference, fetch_url, index_dir) = if reference.eq_ignore_ascii_case("grch38")
        || reference.eq_ignore_ascii_case("hg38")
    {
        let p = cached_reference_path(&ctx.cache_dir);
        let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
        (p, Some(ctx.get("reference_url").to_string()), dir)
    } else if reference.eq_ignore_ascii_case("grch37") || reference.eq_ignore_ascii_case("hg19") {
        return Err(AppError::usage("only GRCh38 can be fetched automatically; pass --reference /path/to/GRCh37.fa"));
    } else {
        let p = std::fs::canonicalize(crate::context::expand_tilde(&reference))
            .map_err(|e| AppError::not_found(format!("reference {reference}: {e}")))?;
        (p, None, std::fs::canonicalize(&a.out).unwrap_or_else(|_| a.out.clone()).join("ref-index"))
    };
    let region = a
        .region
        .as_deref()
        .map(|r| Region::parse(r).ok_or_else(|| AppError::usage(format!("bad region '{r}' (e.g. chr19:44.9M-45.0M)"))))
        .transpose()?;
    let threads = a.threads.unwrap_or_else(|| ctx.get("threads").parse().unwrap_or(4)).max(1);
    let caller = a.caller.clone().unwrap_or_else(|| ctx.get("caller").to_string());
    let aligner = a.aligner.clone().unwrap_or_else(|| ctx.get("aligner").to_string());
    let container = match ctx.get("container") {
        "auto" => if which("docker").is_some() || which("podman").is_none() { "docker" } else { "podman" }.to_string(),
        c => c.to_string(),
    };
    let sample = a.sample.clone().unwrap_or_else(|| lanes[0].sample.clone());
    let out = if a.out.is_absolute() { a.out.clone() } else { std::env::current_dir()?.join(&a.out) };
    Ok(Settings {
        out,
        reference,
        fetch_url,
        index_dir,
        region,
        max_reads: a.max_reads,
        threads,
        caller,
        aligner,
        container,
        name: a.name.clone().unwrap_or_else(|| sample.clone()),
        sample,
        import: !a.no_import,
    })
}

pub fn run_cmd(ctx: &Ctx, cmd: PipelineCmd) -> Result<i32> {
    let (run, a) = match cmd {
        PipelineCmd::Plan(a) => (false, a),
        PipelineCmd::Run(a) => (true, a),
    };
    let fastq: Vec<PathBuf> = a.fastq.iter().map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    let lanes = pair_lanes(&fastq)?;
    let st = settings(ctx, &a, &lanes)?;
    let mut steps = plan(&st, &lanes);
    mark_cached(&mut steps, a.force);
    let mut warnings = Vec::new();
    if st.region.is_some() {
        warnings.push("--region restricts variant calling; the whole reference is still indexed once (cached)".into());
    }
    let missing: Vec<String> = steps
        .iter()
        .filter(|s| s.builtin.is_none() && s.status == "planned")
        .map(|s| s.tool.clone())
        .filter(|t| which(t).is_none())
        .fold(Vec::new(), |mut acc, t| {
            if !acc.contains(&t) {
                acc.push(t);
            }
            acc
        });
    for t in &missing {
        warnings.push(format!("{t} not found on PATH (install: {})", brew_hint(t)));
    }
    if !run {
        let rows = steps.iter().map(Step::record).collect();
        return ctx.emit(&pipeline_report("pipeline-plan", rows, warnings)).map(|()| 0);
    }
    if !missing.is_empty() {
        let rows = steps.iter().map(Step::record).collect();
        let _ = ctx.emit(&pipeline_report("pipeline-run", rows, warnings));
        return Err(AppError::tool(format!("missing tools: {} (see `genome doctor`)", missing.join(", "))));
    }
    std::fs::create_dir_all(&st.out)?;
    let logs = st.out.join("logs");
    std::fs::create_dir_all(&logs)?;
    let mut err = None;
    let mut force_rest = a.force;
    for g in groups(&steps) {
        let (i, o) = group_io(&steps, &g);
        if !force_rest && up_to_date(&i, &o) {
            steps[g].iter_mut().for_each(|s| s.status = "skipped-cached");
            continue;
        }
        force_rest = true;
        let start = Instant::now();
        let first = &steps[g.start];
        ctx.info(&format!("[{}] {}", first.step, shell_join(&first.argv)));
        let res = match first.builtin.clone() {
            Some(Builtin::FetchReference { url, gz, dest }) => fetch_reference(&url, &gz, &dest, ctx),
            Some(Builtin::Subsample { r1, r2, n, out1, out2 }) => head_reads(&r1, &r2, n, &out1, &out2),
            Some(Builtin::Import { vcf, name, marker }) => {
                let args = ImportArgs {
                    file: vcf,
                    name: Some(name),
                    input_format: crate::parse::InputFormat::Vcf,
                    sample: None,
                    build: None,
                    assay: Some("wgs".into()),
                    ref_calls: None,
                    fastq_derived: true,
                    replace: true,
                };
                // The pipeline report is the command's output: import quietly.
                let quiet_ctx = Ctx {
                    out: crate::output::OutputOpts { output: Some(PathBuf::from("/dev/null")), ..ctx.out.clone() },
                    ..ctx.clone()
                };
                crate::commands::import::run(&quiet_ctx, args).and_then(|kit| {
                    std::fs::write(&marker, serde_json::to_string_pretty(&json!({"kit": kit.id, "name": kit.name}))?)?;
                    ctx.info(&format!("imported {} '{}' ({} records)", kit.id, kit.name, kit.records));
                    Ok(())
                })
            }
            None => run_group(&steps[g.clone()], &logs, g.start),
        };
        let secs = start.elapsed().as_secs_f64();
        let ok = res.is_ok();
        steps[g].iter_mut().for_each(|s| {
            s.status = if ok { "done" } else { "failed" };
            s.seconds = Some(secs);
        });
        if let Err(e) = res {
            err = Some(e);
            break;
        }
    }
    let rows = steps.iter().map(Step::record).collect();
    ctx.emit(&pipeline_report("pipeline-run", rows, warnings))?;
    match err {
        Some(e) => Err(e),
        None => Ok(0),
    }
}

fn pipeline_report(kind: &'static str, rows: Vec<Record>, warnings: Vec<String>) -> Report {
    Report::new(kind, rows).table_columns(&["step", "tool", "status", "seconds", "command"]).warnings(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_multilane() {
        let files: Vec<PathBuf> = [
            "/d/NA_S1_L002_R2_001.fastq.gz",
            "/d/NA_S1_L001_R1_001.fastq.gz",
            "/d/NA_S1_L001_R2_001.fastq.gz",
            "/d/NA_S1_L002_R1_001.fastq.gz",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let lanes = pair_lanes(&files).unwrap();
        assert_eq!(lanes.len(), 2);
        assert_eq!((lanes[0].id.as_str(), lanes[0].sample.as_str()), ("L001", "NA"));
        assert!(lanes[0].r2.ends_with("NA_S1_L001_R2_001.fastq.gz"));
        let simple = pair_lanes(&[PathBuf::from("x_1.fq.gz"), PathBuf::from("x_2.fq.gz")]).unwrap();
        assert_eq!((simple[0].sample.as_str(), simple[0].id.as_str()), ("x", "L001"));
        assert!(pair_lanes(&[PathBuf::from("x_R1.fastq")]).is_err());
    }

    #[test]
    fn plan_has_every_step_kind() {
        let lanes =
            pair_lanes(&[PathBuf::from("/d/S_L001_R1_001.fastq.gz"), PathBuf::from("/d/S_L001_R2_001.fastq.gz")])
                .unwrap();
        let st = Settings {
            out: PathBuf::from("/out"),
            reference: PathBuf::from("/cache/ref.fna"),
            fetch_url: Some("https://x/ref.fna.gz".into()),
            index_dir: PathBuf::from("/cache"),
            region: Region::parse("chr19:44.9M-45.0M"),
            max_reads: Some(1000),
            threads: 2,
            caller: "bcftools".into(),
            aligner: "minimap2".into(),
            container: "docker".into(),
            sample: "S".into(),
            name: "S".into(),
            import: true,
        };
        let steps = plan(&st, &lanes);
        let kinds: Vec<&str> = steps.iter().map(|s| s.step).collect();
        for k in ["fetch-reference", "index", "align", "sort", "markdup", "call", "filter", "normalize", "import"] {
            assert!(kinds.contains(&k), "missing {k}");
        }
        let mpileup = steps.iter().find(|s| s.argv.get(1).is_some_and(|a| a == "mpileup")).unwrap();
        assert!(mpileup.argv.contains(&"chr19:44900000-45000000".to_string()));
        assert!(mpileup.pipe_next);
        assert_eq!(groups(&steps).len(), steps.len() - 3);
    }

    #[test]
    fn freshness() {
        let d = tempfile::tempdir().unwrap();
        let (i, o) = (d.path().join("in"), d.path().join("out"));
        std::fs::write(&i, "x").unwrap();
        assert!(!up_to_date(std::slice::from_ref(&i), std::slice::from_ref(&o)));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&o, "y").unwrap();
        assert!(up_to_date(std::slice::from_ref(&i), std::slice::from_ref(&o)));
    }
}
