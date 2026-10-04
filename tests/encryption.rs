//! Encryption at rest: nothing personal reaches disk in plaintext; wrong keys,
//! tampering, rekey and plaintext migration behave. See docs/security.md.

use std::path::{Path, PathBuf};
use std::time::Instant;

use assert_cmd::Command;
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Strings that must never appear in any file genome-cli writes (encrypted mode).
const MARKERS: &[&str] = &["ZQXMARKERKIT", "rs7412", "rs429358", "23andme_male", "SYNTH38", "explicit"];

struct Env {
    dir: TempDir,
    key: String,
}

impl Env {
    fn new() -> Self {
        let e = Self { dir: TempDir::new().unwrap(), key: "test-passphrase".into() };
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
            .env("GENOME_KEY", &self.key)
            .env("GENOME_INSECURE_FAST_KDF", "1")
            .env("NO_COLOR", "1");
        c
    }

    fn run(&self, args: &[&str]) -> (String, String) {
        let out = self.cmd().args(args).output().unwrap();
        let (o, e) =
            (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        assert!(out.status.success(), "genome {args:?} failed ({:?}):\n{o}\n{e}", out.status.code());
        (o, e)
    }

    /// Run expecting failure; returns (exit code, stderr).
    fn fail(&self, c: &mut Command, args: &[&str]) -> (i32, String) {
        let out = c.args(args).output().unwrap();
        assert!(!out.status.success(), "genome {args:?} unexpectedly succeeded");
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn populate(&self, extra: &[&str]) {
        let a = fixture("23andme_male.txt");
        let v = fixture("wgs_grch38.vcf.gz");
        let mut args = vec!["import", a.to_str().unwrap(), "--name", "ZQXMARKERKIT"];
        args.extend(extra);
        self.run(&args);
        let mut args = vec!["import", v.to_str().unwrap(), "--name", "wgs"];
        args.extend(extra);
        self.run(&args);
        for args in [
            vec!["lookup", "ZQXMARKERKIT", "--rsid", "rs7412,rs429358"],
            vec!["lookup", "wgs", "--rsid", "rs7412"],
            vec!["summary"],
            vec!["compare", "wgs", "ZQXMARKERKIT"],
            vec!["export", "wgs", "--format", "vcf"],
        ] {
            let mut a = args.clone();
            a.extend(extra);
            self.run(&a);
        }
    }
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// (file, marker) pairs found in plaintext under `dirs`.
fn scan(dirs: &[PathBuf], markers: &[&str]) -> Vec<(PathBuf, String)> {
    let mut hits = Vec::new();
    for d in dirs {
        for f in files(d) {
            let bytes = std::fs::read(&f).unwrap_or_default();
            for m in markers {
                if contains(&bytes, m.as_bytes()) {
                    hits.push((f.clone(), (*m).to_string()));
                }
            }
        }
    }
    hits
}

fn flip(p: &Path, at_from_end: usize) {
    let mut b = std::fs::read(p).unwrap();
    let n = b.len();
    b[n - at_from_end] ^= 0x01;
    std::fs::write(p, b).unwrap();
}

/// Step 1 of the encryption work: does fsqlite's `PRAGMA fsqlite.key` (documented on
/// frankensqlite.com) encrypt? In fsqlite 0.4.9 it does not: the cipher exists in
/// fsqlite-pager but no PRAGMA dispatch reaches it and unknown pragmas are ignored,
/// so the marker is found in plaintext. If this test starts failing, fsqlite has
/// wired page encryption: re-evaluate docs/security.md.
#[test]
fn fsqlite_pragma_key_does_not_encrypt() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("probe.db");
    let p = path.clone();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let db = genome_cli::db::Db::open_raw(&p).unwrap();
            for pragma in ["PRAGMA fsqlite.key = 'probe-passphrase'", "PRAGMA key = 'probe-passphrase'"] {
                // Accepted or rejected, it must not matter: the result below is what counts.
                let _ = db.execute(pragma, &[]);
            }
            db.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
            db.execute("INSERT INTO t (v) VALUES ('FSQLITE_PLAINTEXT_MARKER_7c1e')", &[]).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
    let hits = scan(&[dir.path().to_path_buf()], &["FSQLITE_PLAINTEXT_MARKER_7c1e"]);
    let names: Vec<String> =
        files(dir.path()).iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
    eprintln!("fsqlite files: {names:?}; marker found in: {hits:?}");
    assert!(!hits.is_empty(), "PRAGMA fsqlite.key now encrypts: revisit docs/security.md and src/db.rs");
}

#[test]
fn no_plaintext_personal_data_on_disk() {
    let e = Env::new();
    e.populate(&[]);
    e.run(&["audit", "log"]);
    e.run(&["doctor"]);
    let dirs = [e.path("data"), e.path("cache")];
    let hits = scan(&dirs, MARKERS);
    assert!(hits.is_empty(), "plaintext personal data on disk: {hits:?}");
    // Every file written is a known, encrypted (or empty lock) file: no WAL/journal/temp sidecars.
    for f in files(&e.path("data")) {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let head = std::fs::read(&f).unwrap();
        match name.as_str() {
            "genome.db.lock" => assert!(head.is_empty()),
            "genome.db" => assert!(head.starts_with(b"GNMDBSE1")),
            "audit.log" => assert!(head.starts_with(b"GNMAUDT1")),
            "sites.bin" | "heap.bin" | "rsid.idx" | "contigs.json" => assert!(head.starts_with(b"GNMSEAL1"), "{f:?}"),
            other => panic!("unexpected file {other} in data dir"),
        }
    }
    // Control: the same session in --insecure-plaintext mode does leave the markers
    // (proves the scan would catch them).
    let p = Env::new();
    p.populate(&["--insecure-plaintext"]);
    let hits = scan(&[p.path("data")], MARKERS);
    for m in ["ZQXMARKERKIT", "rs7412"] {
        assert!(hits.iter().any(|(_, x)| x == m), "control scan missed {m}: {hits:?}");
    }
}

#[test]
fn wrong_key_and_missing_key_fail_clearly() {
    let e = Env::new();
    e.populate(&[]);
    let (code, err) = e.fail(e.cmd().env("GENOME_KEY", "not-the-key"), &["kits"]);
    assert_eq!(code, 10, "{err}");
    assert!(err.contains("wrong key"), "{err}");
    let (code, err) = e.fail(e.cmd().env_remove("GENOME_KEY"), &["kits"]);
    assert_eq!(code, 10, "{err}");
    assert!(err.contains("GENOME_KEY"), "{err}");
    let (code, err) = e.fail(e.cmd().arg("--format").arg("json"), &["lookup", "nope", "--rsid", "rs1"]);
    assert_eq!(code, 3, "{err}");
}

#[test]
fn tampering_is_detected() {
    let e = Env::new();
    e.populate(&[]);
    let check = |file: PathBuf, from_end: usize, args: &[&str]| {
        let orig = std::fs::read(&file).unwrap();
        flip(&file, from_end);
        let (code, err) = e.fail(&mut e.cmd(), args);
        assert_eq!(code, 10, "{file:?}: {err}");
        assert!(err.contains("decryption failed") || err.contains("verification"), "{file:?}: {err}");
        std::fs::write(&file, orig).unwrap();
        e.run(args);
    };
    check(e.path("data/genome.db"), 5, &["kits"]);
    check(e.path("data/kits/k1/heap.bin"), 30, &["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]);
    check(e.path("data/kits/k1/sites.bin"), 30, &["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]);
    check(e.path("data/kits/k1/rsid.idx"), 30, &["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]);
    check(e.path("data/kits/k1/contigs.json"), 30, &["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]);
    check(e.path("data/audit.log"), 30, &["audit", "log"]);
    // Swapping two stores' files (same key, different role) is detected too.
    let (s, h) = (e.path("data/kits/k1/sites.bin"), e.path("data/kits/k1/heap.bin"));
    let (sb, hb) = (std::fs::read(&s).unwrap(), std::fs::read(&h).unwrap());
    std::fs::write(&s, &hb).unwrap();
    let (code, _) = e.fail(&mut e.cmd(), &["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]);
    assert_eq!(code, 10);
    std::fs::write(&s, sb).unwrap();
    std::fs::write(&h, hb).unwrap();
}

#[test]
fn rekey_changes_the_passphrase() {
    let e = Env::new();
    e.populate(&[]);
    let before = e.run(&["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"]).0;
    e.cmd().env("GENOME_NEW_KEY", "second-passphrase").args(["db", "rekey"]).assert().success();
    let (code, err) = e.fail(&mut e.cmd(), &["kits"]);
    assert_eq!(code, 10, "{err}");
    let after = e
        .cmd()
        .env("GENOME_KEY", "second-passphrase")
        .args(["lookup", "ZQXMARKERKIT", "--rsid", "rs7412"])
        .output()
        .unwrap();
    assert!(after.status.success());
    assert_eq!(String::from_utf8_lossy(&after.stdout), before);
}

#[test]
fn migrate_plaintext_database_in_place() {
    let e = Env::new();
    e.populate(&["--insecure-plaintext"]);
    let before = e.run(&["--insecure-plaintext", "lookup", "ZQXMARKERKIT", "--rsid", "rs7412,rs429358"]).0;
    // Without the flag a plaintext database is refused.
    let (code, err) = e.fail(&mut e.cmd(), &["kits"]);
    assert_eq!(code, 10, "{err}");
    assert!(err.contains("db encrypt"), "{err}");
    e.run(&["db", "encrypt"]);
    assert_eq!(e.run(&["lookup", "ZQXMARKERKIT", "--rsid", "rs7412,rs429358"]).0, before);
    let hits = scan(&[e.path("data")], MARKERS);
    assert!(hits.is_empty(), "plaintext left after db encrypt: {hits:?}");
    let leftovers: Vec<_> = files(&e.path("data"))
        .into_iter()
        .filter(|f| f.file_name().unwrap().to_string_lossy().starts_with("genome.db-"))
        .collect();
    assert!(leftovers.is_empty(), "plaintext sidecars left: {leftovers:?}");
    let audit = e.run(&["audit", "log", "--format", "json"]).0;
    assert!(audit.contains("\"db encrypt\"") && audit.contains("\"import\""), "{audit}");
    // Idempotent.
    e.run(&["db", "encrypt"]);
}

#[test]
fn exports_warn_and_encrypt() {
    let e = Env::new();
    e.populate(&[]);
    let plain = e.path("plain.vcf");
    let (_, err) = e.run(&["export", "wgs", "--format", "vcf", "-o", plain.to_str().unwrap()]);
    assert!(err.contains("writing plaintext health data"), "{err}");
    let sealed = e.path("sealed.vcf.enc");
    let out = e
        .cmd()
        .env("GENOME_EXPORT_KEY", "export-pass")
        .args(["export", "wgs", "--format", "vcf", "--encrypt-output", "-o", sealed.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("plaintext health data"));
    assert!(scan(&[e.path("sealed.vcf.enc")], &["SYNTH38", "#CHROM"]).is_empty());
    let dec =
        e.cmd().env("GENOME_EXPORT_KEY", "export-pass").args(["decrypt", sealed.to_str().unwrap()]).output().unwrap();
    assert!(dec.status.success());
    assert_eq!(dec.stdout, std::fs::read(&plain).unwrap());
    let (code, _) = e.fail(e.cmd().env("GENOME_EXPORT_KEY", "bad"), &["decrypt", sealed.to_str().unwrap()]);
    assert_eq!(code, 10);
}

#[test]
fn audit_log_records_commands_without_values() {
    let e = Env::new();
    e.populate(&[]);
    let v: serde_json::Value = serde_json::from_str(&e.run(&["audit", "log", "--format", "json"]).0).unwrap();
    let cmds: Vec<&str> = v["data"].as_array().unwrap().iter().map(|r| r["command"].as_str().unwrap()).collect();
    assert_eq!(cmds, ["import", "import", "lookup", "lookup", "summary", "compare", "export"]);
    let text = v["data"].to_string();
    for m in ["ZQXMARKERKIT", "rs7412", "TC", "CC"] {
        assert!(!text.contains(&format!("\"{m}\"")), "audit log leaks {m}: {text}");
    }
}

#[test]
fn insecure_plaintext_warns_and_status_reports() {
    let e = Env::new();
    let (_, err) = e.run(&["--insecure-plaintext", "db", "init"]);
    assert!(err.contains("UNENCRYPTED"), "{err}");
    let doctor = e.run(&["--insecure-plaintext", "doctor"]).0;
    assert!(doctor.contains("PLAINTEXT"), "{doctor}");
    let s = Env::new();
    let status = s.run(&["db", "init"]).0;
    assert!(status.contains("encrypted") && status.contains("passphrase"), "{status}");
    let (code, _) = s.fail(&mut s.cmd(), &["db", "init"]);
    assert_eq!(code, 2);
}

#[test]
fn pipeline_plan_with_seal() {
    let e = Env::new();
    let out = e.run(&[
        "pipeline",
        "plan",
        "S_L001_R1_001.fastq.gz",
        "S_L001_R2_001.fastq.gz",
        "--out",
        e.path("run").to_str().unwrap(),
        "--seal",
        "--columns",
        "step,status",
    ]);
    assert!(out.0.lines().last().unwrap().starts_with("seal"), "{}", out.0);
}

/// Performance smoke test: a 100k-record VCF imports, looks up and exports in
/// encrypted mode within a small factor of plaintext mode (debug build).
#[test]
fn performance_smoke() {
    let e = Env::new();
    let vcf = e.path("big.vcf");
    let mut s = String::from("##fileformat=VCFv4.2\n##contig=<ID=chr1,length=248956422>\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\tS\n");
    for i in 0..100_000u32 {
        s.push_str(&format!("chr1\t{}\trs{}\tA\tG\t50\tPASS\t.\tGT\t0/1\n", 1000 + i * 10, 900_000 + i));
    }
    std::fs::write(&vcf, s).unwrap();
    let time = |env: &Env, extra: &[&str]| {
        let t = Instant::now();
        let mut a = vec!["import", vcf.to_str().unwrap(), "--name", "big", "--quiet"];
        a.extend(extra);
        env.run(&a);
        let import = t.elapsed();
        let t = Instant::now();
        let mut a = vec!["lookup", "big", "--pos", "1:500000", "--rsid", "rs950000"];
        a.extend(extra);
        let out = env.run(&a).0;
        assert!(out.contains("rs950000"), "{out}");
        let lookup = t.elapsed();
        let t = Instant::now();
        let mut a = vec!["export", "big", "--format", "vcf"];
        a.extend(extra);
        assert_eq!(env.run(&a).0.lines().filter(|l| !l.starts_with('#')).count(), 100_000);
        (import, lookup, t.elapsed())
    };
    let sealed = time(&e, &[]);
    let plain = time(&Env::new(), &["--insecure-plaintext"]);
    eprintln!("100k records, debug build: encrypted (import, lookup, export) = {sealed:?}; plaintext = {plain:?}");
    assert!(sealed.0 < plain.0 * 3 + std::time::Duration::from_secs(5), "import too slow: {sealed:?} vs {plain:?}");
    assert!(sealed.1 < std::time::Duration::from_secs(3), "lookup too slow: {sealed:?}");
    assert!(sealed.2 < plain.2 * 3 + std::time::Duration::from_secs(5), "export too slow: {sealed:?} vs {plain:?}");
}

#[cfg(unix)]
#[test]
fn data_dir_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    e.populate(&[]);
    let mode = std::fs::metadata(e.path("data")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "data dir mode {mode:o}");
}
