//! Measures bulk genotype-row inserts and point lookups in fsqlite.
//!
//! `cargo run --release --example fsqlite_bench -- 200000`
use std::time::Instant;

use asupersync::runtime::RuntimeBuilder;
use fsqlite::{Connection, SqliteValue as V};

fn main() {
    let n: i64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.db").to_string_lossy().into_owned();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let rt = RuntimeBuilder::current_thread().build().unwrap();
            let c = rt.block_on(Connection::open(path)).unwrap();
            rt.block_on(c.execute(
                "CREATE TABLE g (chrom INTEGER, pos INTEGER, rsid TEXT, ref TEXT, alt TEXT, gt TEXT, PRIMARY KEY (chrom, pos))",
            ))
            .unwrap();
            let t = Instant::now();
            rt.block_on(c.begin_transaction()).unwrap();
            for i in 0..n {
                let p = [V::Integer(1 + i % 22), V::Integer(i * 7), V::from(format!("rs{i}").as_str()), V::from("A"), V::from("G"), V::from("0/1")];
                rt.block_on(c.execute_with_params("INSERT INTO g VALUES (?1, ?2, ?3, ?4, ?5, ?6)", &p)).unwrap();
            }
            rt.block_on(c.commit_transaction()).unwrap();
            let ins = t.elapsed().as_secs_f64();
            let t = Instant::now();
            for i in 0..1000 {
                let j = (i * 7919) % n;
                let p = [V::Integer(1 + j % 22), V::Integer(j * 7)];
                let r = rt.block_on(c.query_with_params("SELECT gt FROM g WHERE chrom = ?1 AND pos = ?2", &p)).unwrap();
                assert_eq!(r.len(), 1);
            }
            let q = t.elapsed().as_secs_f64();
            println!("rows={n} insert_s={ins:.2} rows_per_s={:.0} lookup_ms_each={:.3}", n as f64 / ins, q);
        })
        .unwrap()
        .join()
        .unwrap();
}
