//! End-to-end FASTQ -> VCF -> kit test on simulated reads.
//!
//! Builds a synthetic 20 kb reference with planted SNPs, simulates paired-end
//! reads from a diploid sample (two lanes), runs `genome pipeline run` with the
//! real tools and checks the imported genotypes. Skips (with a message) when
//! minimap2, samtools or bcftools are not on PATH.

use std::io::Write;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const BASES: [u8; 4] = *b"ACGT";

fn which(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn revcomp(s: &[u8]) -> Vec<u8> {
    s.iter()
        .rev()
        .map(|b| match b {
            b'A' => b'T',
            b'C' => b'G',
            b'G' => b'C',
            _ => b'A',
        })
        .collect()
}

/// (pos 1-based, ref, alt, hom?)
fn plant(reference: &[u8], rng: &mut Rng) -> Vec<(usize, u8, u8, bool)> {
    (0..8)
        .map(|i| {
            let pos = 1500 + i * 2200;
            let r = reference[pos - 1];
            let alt = loop {
                let b = BASES[rng.below(4) as usize];
                if b != r {
                    break b;
                }
            };
            (pos, r, alt, i % 3 == 0)
        })
        .collect()
}

fn write_fastq_gz(path: &Path, reads: &[(String, Vec<u8>)]) {
    let f = std::fs::File::create(path).unwrap();
    let mut w = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
    for (name, seq) in reads {
        writeln!(w, "@{name}\n{}\n+\n{}", String::from_utf8_lossy(seq), "I".repeat(seq.len())).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn fastq_to_kit_with_real_tools() {
    let missing: Vec<&str> = ["minimap2", "samtools", "bcftools"].into_iter().filter(|t| !which(t)).collect();
    if !missing.is_empty() {
        eprintln!("SKIPPED pipeline e2e: missing {} on PATH (see `genome doctor`)", missing.join(", "));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let len = 20_000;
    let reference: Vec<u8> = (0..len).map(|_| BASES[rng.below(4) as usize]).collect();
    let snps = plant(&reference, &mut rng);
    let ref_path = d.join("synth.fa");
    let mut fa = std::fs::File::create(&ref_path).unwrap();
    writeln!(fa, ">chr22 synthetic").unwrap();
    for chunk in reference.chunks(60) {
        fa.write_all(chunk).unwrap();
        fa.write_all(b"\n").unwrap();
    }
    drop(fa);
    // Two haplotypes: hom SNPs on both, het SNPs on hap 1 only.
    let mut haps = [reference.clone(), reference.clone()];
    for (pos, _, alt, hom) in &snps {
        haps[1][pos - 1] = *alt;
        if *hom {
            haps[0][pos - 1] = *alt;
        }
    }
    let (read_len, pairs) = (100usize, 4000usize);
    let reads_dir = d.join("reads");
    std::fs::create_dir_all(&reads_dir).unwrap();
    let mut files = Vec::new();
    for lane in ["L001", "L002"] {
        let (mut r1, mut r2) = (Vec::new(), Vec::new());
        for i in 0..pairs / 2 {
            let hap = &haps[rng.below(2) as usize];
            let insert = 280 + rng.below(40) as usize;
            let start = rng.below((len - insert) as u64) as usize;
            let frag = &hap[start..start + insert];
            let mut a = frag[..read_len].to_vec();
            let mut b = revcomp(&frag[insert - read_len..]);
            // 0.1% substitution errors.
            for s in [&mut a, &mut b] {
                for base in s.iter_mut() {
                    if rng.below(1000) == 0 {
                        *base = BASES[rng.below(4) as usize];
                    }
                }
            }
            let name = format!("SYN:{lane}:{i}");
            r1.push((format!("{name}/1"), a));
            r2.push((format!("{name}/2"), b));
        }
        for (r, reads) in [("R1", &r1), ("R2", &r2)] {
            let p = reads_dir.join(format!("SYN_S1_{lane}_{r}_001.fastq.gz"));
            write_fastq_gz(&p, reads);
            files.push(p);
        }
    }
    let out = d.join("run");
    let run = |extra: &[&str]| -> Value {
        let mut c = Command::cargo_bin("genome").unwrap();
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", d)
            .env("XDG_CONFIG_HOME", d.join("config"))
            .env("GENOME_DATA_DIR", d.join("data"))
            .env("GENOME_CACHE_DIR", d.join("cache"))
            .env("GENOME_OFFLINE", "1")
            .args(["pipeline", "run"])
            .args(files.iter().map(|p| p.as_os_str()))
            .args(["--out", out.to_str().unwrap(), "--reference", ref_path.to_str().unwrap(), "--threads", "2"])
            .args(["--format", "json"])
            .args(extra);
        let o = c.output().unwrap();
        assert!(
            o.status.success(),
            "pipeline failed:\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let v = run(&[]);
    assert_eq!(v["kind"], "pipeline-run");
    for s in v["data"].as_array().unwrap() {
        assert_eq!(s["status"], "done", "{s}");
        assert!(s["seconds"].is_number());
    }
    assert!(out.join("SYN.vcf.gz").exists());
    // Second run: everything is up to date.
    let again = run(&[]);
    assert!(again["data"].as_array().unwrap().iter().all(|s| s["status"] == "skipped-cached"), "{again}");

    let lookup = |pos: &str| -> Value {
        let mut c = Command::cargo_bin("genome").unwrap();
        c.env_clear()
            .env("HOME", d)
            .env("XDG_CONFIG_HOME", d.join("config"))
            .env("GENOME_DATA_DIR", d.join("data"))
            .env("GENOME_CACHE_DIR", d.join("cache"))
            .args(["lookup", "SYN", "--pos", pos, "--format", "json"]);
        let o = c.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let kits: Value = {
        let o = Command::cargo_bin("genome")
            .unwrap()
            .env_clear()
            .env("HOME", d)
            .env("XDG_CONFIG_HOME", d.join("config"))
            .env("GENOME_DATA_DIR", d.join("data"))
            .args(["kits", "--format", "json"])
            .output()
            .unwrap();
        serde_json::from_slice(&o.stdout).unwrap()
    };
    assert_eq!(kits["data"][0]["source_format"], "fastq-derived");
    // Our variant-only, possibly region/subsample-limited calls never imply hom-ref.
    assert_eq!(kits["data"][0]["ref_calls"], "unknown");
    for (pos, r, alt, hom) in &snps {
        let v = lookup(&format!("chr22:{pos}"));
        let row = &v["data"][0];
        assert_eq!(row["call_source"], "observed", "SNP at {pos}: {row}");
        assert_eq!(row["ref"], String::from(*r as char));
        assert_eq!(row["zygosity"], if *hom { "hom_alt" } else { "het" }, "SNP at {pos}: {row}");
        let mut expect = if *hom { vec![*alt, *alt] } else { vec![*r, *alt] };
        let mut got = row["genotype"].as_str().unwrap().as_bytes().to_vec();
        expect.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expect, "SNP at {pos}");
    }
    // A site between SNPs is not claimed as reference: coverage is unknown.
    let v = lookup("chr22:1000");
    assert_eq!(v["data"][0]["call_source"], "missing");
}
