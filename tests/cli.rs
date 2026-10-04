//! End-to-end tests driving the `genome` binary on synthetic fixtures.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        let e = Self { dir: TempDir::new().unwrap() };
        // Synthetic chains stand in for the UCSC files (no network in tests).
        let chains = e.path("cache/chains");
        std::fs::create_dir_all(&chains).unwrap();
        std::fs::copy(fixture("hg19ToHg38.chain"), chains.join("hg19ToHg38.over.chain.gz")).unwrap();
        std::fs::copy(fixture("hg38ToHg19.chain"), chains.join("hg38ToHg19.over.chain.gz")).unwrap();
        e
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("genome").unwrap();
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("GENOME_DATA_DIR", self.path("data"))
            .env("GENOME_CACHE_DIR", self.path("cache"))
            .env("GENOME_OFFLINE", "1")
            .env("NO_COLOR", "1");
        c
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "genome {args:?} failed ({:?}):\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.extend(["--format", "json"]);
        let v: Value = serde_json::from_str(&self.run(&a)).unwrap();
        assert_eq!(v["schema"], "genome/v1");
        assert!(v["warnings"].is_array());
        assert!(v["generated_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["count"].as_u64().unwrap() as usize, v["data"].as_array().unwrap().len());
        v
    }

    fn import(&self, file: &str, name: &str) -> Value {
        let f = fixture(file);
        let v = self.json(&["import", f.to_str().unwrap(), "--name", name]);
        assert_eq!(v["kind"], "kits");
        v["data"][0].clone()
    }
}

#[test]
fn import_arrays_each_format() {
    let e = Env::new();
    for (file, fmt) in [
        ("23andme_male.txt", "23andme"),
        ("ancestry.txt", "ancestry"),
        ("myheritage.csv", "myheritage"),
        ("ftdna.csv", "ftdna"),
    ] {
        let k = e.import(file, fmt);
        assert_eq!(k["source_format"], fmt, "{file}");
        assert_eq!(k["assay"], "array");
        assert_eq!(k["build"], "GRCh37");
        assert_eq!(k["has_rsids"], true);
        assert_eq!(k["ref_calls"], "explicit");
        assert_eq!(k["chip"], "unknown");
        let expect = if fmt == "ftdna" { "assumed" } else { "header" };
        assert_eq!(k["build_evidence"], expect, "{file}");
    }
    let kits = e.json(&["kits"]);
    assert_eq!(kits["count"], 4);
    let keys: Vec<&str> = kits["data"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "id",
            "name",
            "source_format",
            "assay",
            "build",
            "build_evidence",
            "sample",
            "records",
            "has_rsids",
            "ref_calls",
            "chip",
            "imported_at",
            "source_path"
        ]
    );
    assert_eq!(kits["data"][0]["id"], "k1");
}

#[test]
fn explicit_input_format_flag() {
    let e = Env::new();
    let f = fixture("ftdna.csv");
    let v = e.json(&["import", f.to_str().unwrap(), "--format", "myheritage", "--name", "x"]);
    // `--format myheritage` is the input format; output falls back to the configured format.
    assert_eq!(v["data"][0]["source_format"], "myheritage");
}

#[test]
fn summary_sex_inference_and_par() {
    let e = Env::new();
    e.import("23andme_male.txt", "m");
    e.import("23andme_female.txt", "f");
    let s = e.json(&["summary", "m", "f"]);
    assert_eq!(s["kind"], "summary");
    let m = &s["data"][0];
    assert_eq!(m["kit"], "k1");
    assert_eq!(m["sex"]["call"], "male", "{m}");
    let y = m["sex"]["y_call_rate"].as_f64().unwrap();
    assert!((0.85..0.88).contains(&y), "y_call_rate {y}");
    // 1 het among 40 non-PAR X sites: the 2 PAR het sites are excluded.
    assert_eq!(m["sex"]["x_het_rate"], 0.025);
    assert_eq!(m["no_calls"], 5);
    assert_eq!(m["by_chrom"]["Y"], 30);
    assert_eq!(m["by_chrom"]["MT"], 1);
    assert_eq!(m["by_chrom"]["other_contigs"], 0);
    assert!(m["hemizygous"].as_u64().unwrap() >= 20);
    let f = &s["data"][1];
    assert_eq!(f["sex"]["call"], "female", "{f}");
    assert_eq!(f["sex"]["y_call_rate"], 0.0);
}

#[test]
fn wgs_vcf_import_and_summary() {
    let e = Env::new();
    let k = e.import("wgs_grch38.vcf.gz", "wgs");
    assert_eq!(k["source_format"], "vcf");
    assert_eq!(k["assay"], "wgs");
    assert_eq!(k["build"], "GRCh38");
    assert_eq!(k["build_evidence"], "contig-lengths");
    assert_eq!(k["has_rsids"], false);
    assert_eq!(k["ref_calls"], "absent-means-ref");
    assert_eq!(k["sample"], "SYNTH38");
    assert_eq!(k["chip"], Value::Null);
    let s = &e.json(&["summary", "wgs"])["data"][0];
    assert_eq!(s["by_chrom"]["other_contigs"], 5);
    assert_eq!(s["by_chrom"]["MT"], 1);
    assert_eq!(s["hemizygous"], 6);
    assert_eq!(s["no_calls"], 1);
    assert_eq!(s["sex"]["call"], "male", "{s}");
    assert!(s["caveats"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().contains("absent")));
}

#[test]
fn gvcf_and_grch37_detection() {
    let e = Env::new();
    let g = e.import("sample.g.vcf", "g");
    assert_eq!(g["source_format"], "gvcf");
    assert_eq!(g["ref_calls"], "explicit");
    let k = e.import("wgs_grch37_rsids.vcf", "b37");
    assert_eq!(k["build"], "GRCh37");
    assert_eq!(k["has_rsids"], true);
    let s = &e.json(&["summary", "b37"])["data"][0];
    assert_eq!(s["by_chrom"]["other_contigs"], 3);
    // Block lookup inside a gVCF reference block is an explicit hom_ref.
    let v = e.json(&["lookup", "g", "--pos", "chr19:44908700"]);
    assert_eq!(v["data"][0]["zygosity"], "hom_ref");
    assert_eq!(v["data"][0]["call_source"], "observed");
    // Outside any block: missing (gVCF absence is not reference).
    let v = e.json(&["lookup", "g", "--pos", "chr19:50000000"]);
    assert_eq!(v["data"][0]["call_source"], "missing");
}

#[test]
fn lookup_by_rsid_on_array() {
    let e = Env::new();
    e.import("23andme_male.txt", "m");
    let v = e.json(&["lookup", "m", "--rsid", "rs429358,rs7412", "--rsid", "rs1801133"]);
    assert_eq!(v["kind"], "genotypes");
    let d = &v["data"];
    assert_eq!(d[0]["rsid"], "rs429358");
    assert_eq!(d[0]["chrom"], "19");
    assert_eq!(d[0]["pos"], 45411941);
    assert_eq!(d[0]["build"], "GRCh37");
    assert_eq!(d[0]["genotype"], "TC");
    assert_eq!(d[0]["zygosity"], "het");
    assert_eq!(d[0]["call_source"], "observed");
    assert_eq!(d[0]["ref"], "T");
    assert_eq!(d[0]["alt"], serde_json::json!(["C"]));
    assert_eq!(d[0]["lifted_from"], Value::Null);
    // Reference allele from the curated table resolves hom_ref.
    assert_eq!(d[1]["zygosity"], "hom_ref");
    assert_eq!(d[2]["genotype"], "AG");
}

#[test]
fn lookup_row_keys_follow_the_contract() {
    let e = Env::new();
    e.import("23andme_male.txt", "m");
    let v = e.json(&["lookup", "m", "--rsid", "rs429358"]);
    let keys: Vec<&str> = v["data"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
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
            "lifted_from"
        ]
    );
    // Unknown rsid: missing row plus a warning.
    let v = e.json(&["lookup", "m", "--rsid", "rs999999999"]);
    assert_eq!(v["data"][0]["call_source"], "missing");
    assert!(!v["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn lookup_wgs_without_rsids_uses_table_and_inferred_ref() {
    let e = Env::new();
    e.import("wgs_grch38.vcf", "wgs");
    let v = e.json(&["lookup", "wgs", "--rsid", "rs429358,rs7412"]);
    let d = &v["data"];
    assert_eq!(d[0]["pos"], 44908684);
    assert_eq!(d[0]["genotype"], "TC");
    assert_eq!(d[0]["call_source"], "observed");
    assert_eq!(d[0]["rsid"], "rs429358");
    assert_eq!(d[0]["quality"], 50.0);
    assert_eq!(d[0]["depth"], 30);
    assert_eq!(d[0]["filter"], "PASS");
    // rs7412 absent from a variant-only WGS VCF -> inferred hom_ref.
    assert_eq!(d[1]["pos"], 44908822);
    assert_eq!(d[1]["call_source"], "inferred_ref");
    assert_eq!(d[1]["zygosity"], "hom_ref");
    assert_eq!(d[1]["genotype"], "CC");
    assert!(v["warnings"].to_string().contains("inferred_ref"));
    // Position lookup fills the rsid from the table.
    let v = e.json(&["lookup", "wgs", "--pos", "chr19:44908684"]);
    assert_eq!(v["data"][0]["rsid"], "rs429358");
}

#[test]
fn lookup_across_builds_lifts() {
    let e = Env::new();
    e.import("wgs_grch38.vcf", "wgs");
    // GRCh37 coordinate of rs429358 against a GRCh38 kit.
    let v = e.json(&["lookup", "wgs", "--pos", "19:45411941", "--build", "GRCh37"]);
    let d = &v["data"][0];
    assert_eq!(d["pos"], 44908684);
    assert_eq!(d["build"], "GRCh38");
    assert_eq!(d["genotype"], "TC");
    assert_eq!(d["lifted_from"], serde_json::json!({"build": "GRCh37", "pos": 45411941}));
}

#[test]
fn liftover_command() {
    let e = Env::new();
    let v = e.json(&["liftover", "19:45411941", "chr1:11856378", "5:100"]);
    assert_eq!(v["kind"], "liftover");
    assert_eq!(v["data"][0]["lifted_pos"], 44908684);
    assert_eq!(v["data"][1]["lifted_pos"], 11796321);
    assert_eq!(v["data"][2]["status"], "unmapped");
    let v = e.json(&["liftover", "--from", "GRCh38", "19:44908822"]);
    assert_eq!(v["data"][0]["lifted_pos"], 45412079);
    let chain = fixture("hg19ToHg38.chain");
    let v = e.json(&["liftover", "--chain", chain.to_str().unwrap(), "2:136608646"]);
    assert_eq!(v["data"][0]["lifted_pos"], 135851076);
}

#[test]
fn compare_array_vs_wgs_across_builds() {
    let e = Env::new();
    e.import("wgs_grch38.vcf", "wgs");
    e.import("23andme_male.txt", "arr");
    let v = e.json(&["compare", "wgs", "arr"]);
    assert_eq!(v["kind"], "compare");
    let d = &v["data"][0];
    assert_eq!(d["a"], "k1");
    assert_eq!(d["b"], "k2");
    assert_eq!(d["build"], "GRCh38");
    assert_eq!(d["lifted"], true);
    // Overlap: rs429358 (TC/TC), rs1801131 (TG vs TT: discordant), rs4988235 (GA/AG),
    // rs7412 (inferred CC vs CC), rs1801133 (inferred GG vs AG: discordant).
    assert_eq!(d["overlap"], 5, "{d}");
    assert_eq!(d["concordant"], 3);
    assert_eq!(d["discordant"], 2);
    assert_eq!(d["discordant_sites"].as_array().unwrap().len(), 2);
    let capped = e.json(&["compare", "wgs", "arr", "--max-discordant", "1"]);
    assert_eq!(capped["data"][0]["discordant_sites"].as_array().unwrap().len(), 1);
    assert!(capped["warnings"].to_string().contains("capped"));
}

#[test]
fn compare_same_build_arrays() {
    let e = Env::new();
    e.import("23andme_male.txt", "a");
    e.import("ftdna.csv", "b");
    let d = &e.json(&["compare", "a", "b"])["data"][0];
    assert_eq!(d["lifted"], false);
    assert!(d["overlap"].as_u64().unwrap() > 30);
}

#[test]
fn export_formats() {
    let e = Env::new();
    e.import("wgs_grch38.vcf", "wgs");
    let vcf = e.run(&["export", "wgs", "--format", "vcf", "--region", "chr19"]);
    assert!(vcf.starts_with("##fileformat=VCFv4.2"));
    let body: Vec<&str> = vcf.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(body.len(), 2);
    assert!(body[0].starts_with("chr19\t44908684\t.\tT\tC\t"));
    let tsv = e.run(&["export", "wgs", "--format", "tsv", "--region", "chrX:1-2000000"]);
    assert_eq!(tsv.lines().count(), 3);
    assert!(tsv.starts_with("kit\trsid\tchrom"));
    let j = e.json(&["export", "wgs", "--region", "chrM"]);
    assert_eq!(j["kind"], "genotypes");
    assert_eq!(j["data"][0]["chrom"], "MT");
    e.import("23andme_male.txt", "arr");
    let vcf = e.run(&["export", "arr", "--format", "vcf", "--region", "19"]);
    assert!(vcf.contains("19\t45411941\trs429358\tT\tC\t.\t.\t.\tGT:DP\t0/1:."), "{vcf}");
}

#[test]
fn export_pages_with_offset_and_limit() {
    let e = Env::new();
    e.import("23andme_male.txt", "arr");
    let all = e.json(&["export", "arr"]);
    let n = all["data"].as_array().unwrap().len();
    assert!(n >= 3, "fixture too small: {n}");
    let page = e.json(&["export", "arr", "--offset", "1", "--limit", "2"]);
    let page = page["data"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0], all["data"][1]);
    assert_eq!(page[1], all["data"][2]);
    let past = e.json(&["export", "arr", "--offset", &n.to_string()]);
    assert_eq!(past["data"].as_array().unwrap().len(), 0);
}

#[test]
fn rsid_table_curated_and_dbsnp_import() {
    let e = Env::new();
    let v = e.json(&["rsid-table"]);
    assert_eq!(v["kind"], "rsid-table");
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter().map(|r| r["rsid"].as_str().unwrap()).collect();
    for want in ["rs429358", "rs7412", "rs1801133", "rs1801131", "rs6025", "rs1800562", "rs4988235", "rs4244285"] {
        assert!(ids.contains(&want), "{want}");
    }
    let db = e.path("dbsnp.vcf");
    std::fs::write(
        &db,
        "##fileformat=VCFv4.2\n##reference=GRCh38.p14\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n\
         NC_000001.11\t20000000\trs55555\tA\tG\t.\t.\t.\n",
    )
    .unwrap();
    let v = e.json(&["rsid-table", "import", db.to_str().unwrap()]);
    assert_eq!(v["data"][0]["rsids"], 1);
    // The WGS kit has a variant at chr1:20000000 with no rsid; dbSNP backfills it.
    e.import("wgs_grch38.vcf", "wgs");
    let l = e.json(&["lookup", "wgs", "--rsid", "rs55555"]);
    assert_eq!(l["data"][0]["pos"], 20000000);
    assert_eq!(l["data"][0]["call_source"], "observed");
}

#[test]
fn errors_are_json_envelopes_with_exit_codes() {
    let e = Env::new();
    let out = e.cmd().args(["summary", "nope", "--format", "json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], "genome/v1");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "not_found");
    let out = e.cmd().args(["import", "/nonexistent.txt"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let out = e.cmd().args(["lookup", "k1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let bad = e.path("bad.txt");
    std::fs::write(&bad, "hello\nworld\n").unwrap();
    let out = e.cmd().args(["import", bad.to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(4));
}

#[test]
fn duplicate_names_and_rm() {
    let e = Env::new();
    e.import("ftdna.csv", "x");
    let out = e.cmd().args(["import", fixture("ftdna.csv").to_str().unwrap(), "--name", "x"]).output().unwrap();
    assert_eq!(out.status.code(), Some(4));
    e.run(&["import", fixture("ftdna.csv").to_str().unwrap(), "--name", "x", "--replace"]);
    assert_eq!(e.json(&["kits"])["count"], 1);
    e.run(&["rm", "x"]);
    assert_eq!(e.json(&["kits"])["count"], 0);
}

#[test]
fn output_formats() {
    let e = Env::new();
    e.import("ftdna.csv", "x");
    let t = e.run(&["kits"]);
    assert!(t.starts_with("ID"));
    let c = e.run(&["kits", "--format", "csv"]);
    assert!(c.starts_with("id,name,source_format"));
    let t = e.run(&["kits", "--format", "tsv", "--columns", "id,build"]);
    assert_eq!(t, "id\tbuild\nk1\tGRCh37\n");
    let l = e.run(&["kits", "--format", "jsonl"]);
    assert_eq!(l.lines().count(), 1);
}

#[test]
fn config_layers() {
    let e = Env::new();
    let v = e.json(&["config", "show", "--effective"]);
    let get = |v: &Value, k: &str| v["data"].as_array().unwrap().iter().find(|r| r["key"] == k).unwrap().clone();
    assert_eq!(get(&v, "caller")["source"], "default");
    assert_eq!(get(&v, "offline")["source"], "env");
    e.run(&["config", "set", "threads", "8"]);
    let v = e.json(&["config", "show", "--effective"]);
    assert_eq!(get(&v, "threads")["value"], "8");
    assert_eq!(get(&v, "threads")["source"], "file");
    let out = e
        .cmd()
        .env("GENOME_THREADS", "2")
        .args(["config", "show", "--effective", "--format", "json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(get(&v, "threads")["value"], "2");
    let v = e.json(&["config", "show", "--effective", "--precision", "1"]);
    assert_eq!(get(&v, "precision")["source"], "flag");
    assert!(e.path("config/genome-cli/config.toml").exists());
}

#[test]
fn pipeline_plan_is_a_dry_run() {
    let e = Env::new();
    let reads = e.path("reads");
    std::fs::create_dir_all(&reads).unwrap();
    let mut files = Vec::new();
    for lane in ["L001", "L002"] {
        for r in ["R1", "R2"] {
            let p = reads.join(format!("SYN_S1_{lane}_{r}_001.fastq.gz"));
            std::fs::write(&p, "").unwrap();
            files.push(p.to_string_lossy().into_owned());
        }
    }
    let out = e.path("pipe-out");
    let mut args = vec!["pipeline", "plan"];
    args.extend(files.iter().map(String::as_str));
    args.extend([
        "--out",
        out.to_str().unwrap(),
        "--region",
        "chr19:44.9M-45.0M",
        "--max-reads",
        "1000",
        "--threads",
        "2",
    ]);
    let v = e.json(&args);
    assert_eq!(v["kind"], "pipeline-plan");
    let steps: Vec<&str> = v["data"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    for k in ["fetch-reference", "index", "align", "sort", "markdup", "call", "filter", "normalize", "import"] {
        assert!(steps.contains(&k), "missing {k} in {steps:?}");
    }
    for s in v["data"].as_array().unwrap() {
        assert_eq!(s["status"], "planned");
        assert_eq!(s["seconds"], Value::Null);
        assert!(!s["argv"].as_array().unwrap().is_empty());
    }
    // Two lanes -> two minimap2 alignments and a merge.
    let mm2 = v["data"].as_array().unwrap().iter().filter(|s| s["tool"] == "minimap2" && s["step"] == "align").count();
    assert_eq!(mm2, 2);
    assert!(v["data"].to_string().contains("samtools\",\"merge"));
    assert!(v["data"].to_string().contains("chr19:44900000-45000000"));
    assert!(!out.exists(), "plan must not create the output directory");
    let dv = e.json(&[&args[..], &["--caller", "deepvariant"]].concat());
    assert!(dv["data"].to_string().contains("run_deepvariant"));
}

#[test]
fn doctor_completions_man() {
    let e = Env::new();
    let v = e.json(&["doctor"]);
    assert_eq!(v["kind"], "doctor");
    assert!(v["data"].as_array().unwrap().iter().any(|r| r["check"] == "samtools"));
    assert!(e.run(&["completions", "bash"]).contains("genome"));
    assert!(e.run(&["completions", "zsh"]).contains("genome"));
    assert!(e.run(&["man"]).contains(".TH"));
    let d = e.path("man");
    e.run(&["man", "--dir", d.to_str().unwrap()]);
    assert!(d.join("genome-lookup.1").exists());
}
