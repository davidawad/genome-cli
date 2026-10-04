//! Cost of encryption at rest (docs/security.md, "Performance").
//!
//! `cargo run --release --example encryption_bench -- 5000000`
//!
//! - genotype store: write N records, 1000 random position lookups, full scan,
//!   plaintext vs sealed (chunked XChaCha20-Poly1305);
//! - kit database: open + re-seal of a sealed container holding 50 kits;
//! - Argon2id key derivation with the default parameters.
use std::time::{Duration, Instant};

use genome_cli::crypto::{derive_key, KdfParams, Key};
use genome_cli::gtstore::{Reader, Writer};
use genome_cli::model::{Call, Zygosity};

fn call(i: u32) -> Call {
    Call {
        chrom: ((i % 22) + 1).to_string(),
        pos: 10_000 + (i / 22) * 37,
        end: 10_000 + (i / 22) * 37,
        rsid: Some(format!("rs{}", 1_000_000 + i)),
        reference: Some("A".into()),
        alt: vec!["G".into()],
        genotype: "AG".into(),
        gt: Some("0/1".into()),
        zygosity: Some(Zygosity::Het),
        filter: Some("PASS".into()),
        quality: Some(50.0),
        depth: Some(30),
        ..Call::default()
    }
}

fn store(n: u32, key: Option<&Key>) -> (Duration, Duration, Duration, u64) {
    let dir = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let mut w = Writer::create(dir.path(), key).unwrap();
    for i in 0..n {
        w.push(&call(i)).unwrap();
    }
    w.finish().unwrap();
    let write = t.elapsed();
    let size: u64 = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    let t = Instant::now();
    let mut r = Reader::open(dir.path(), key).unwrap();
    let mut x: u64 = 12345;
    for _ in 0..1000 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let i = ((x >> 33) % u64::from(n)) as u32;
        let c = call(i);
        assert_eq!(r.at(&c.chrom, c.pos).unwrap().len(), 1);
    }
    let lookup = t.elapsed() / 1000;
    let t = Instant::now();
    let mut count = 0u64;
    r.for_each(&mut |_| {
        count += 1;
        Ok(true)
    })
    .unwrap();
    assert_eq!(count, u64::from(n));
    (write, lookup, t.elapsed(), size)
}

fn main() {
    let n: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let key = Key::random().unwrap();
            for (label, k) in [("plaintext", None), ("sealed", Some(&key))] {
                let (w, l, s, size) = store(n, k);
                println!(
                    "store {label:9} n={n}: write {:.2}s, lookup {:.1} us, full scan {:.2}s, {:.0} MB",
                    w.as_secs_f64(),
                    l.as_secs_f64() * 1e6,
                    s.as_secs_f64(),
                    size as f64 / 1e6
                );
            }
            sealed_db(&key);
            let t = Instant::now();
            derive_key(b"correct horse battery staple", b"0123456789abcdef", KdfParams::DEFAULT).unwrap();
            println!("argon2id m=64MiB t=3 p=1: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        })
        .unwrap()
        .join()
        .unwrap();
}

fn sealed_db(dek: &Key) {
    use genome_cli::db::{int, text, Db};
    std::env::set_var("GENOME_KEY", "bench");
    std::env::set_var("GENOME_INSECURE_FAST_KDF", "1");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("genome.db");
    let (env, _) = genome_cli::keys::Envelope::create("passphrase").unwrap();
    let db = Db::open_sealed(&path, env.clone(), dek.clone()).unwrap();
    let summary = format!("{{\"by_chrom\": \"{}\"}}", "x".repeat(4000));
    for i in 1..=50 {
        db.execute(
            "INSERT INTO kits (seq, id, name, source_format, assay, build, build_evidence, records, has_rsids, ref_calls, \
             imported_at, source_path, store_dir, summary_json) VALUES (?1, ?2, ?3, 'vcf', 'wgs', 'GRCh38', 'header', \
             5000000, 0, 'absent-means-ref', 'now', '/x', '/y', ?4)",
            &[int(i), text(format!("k{i}")), text(format!("kit{i}")), text(&summary)],
        )
        .unwrap();
    }
    drop(db);
    let size = std::fs::metadata(&path).unwrap().len();
    let t = Instant::now();
    let db = Db::open_sealed(&path, env, dek.clone()).unwrap();
    let open = t.elapsed();
    let t = Instant::now();
    db.persist().unwrap();
    println!(
        "sealed kit db, 50 kits ({:.0} KB): open {:.1} ms, re-seal (commit) {:.1} ms",
        size as f64 / 1e3,
        open.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );
}
