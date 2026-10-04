//! Filesystem behaviour the CLI relies on and that differs between Unix and
//! Windows: rename over an existing (even open) file, advisory locks held by
//! one handle excluding another (mandatory `LockFileEx` on Windows), and
//! appending to a locked audit log.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

use genome_cli::crypto::{Key, SealedReader, SealedWriter};
use serde_json::json;

fn no_temp_files(dir: &Path) {
    let names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.contains(".tmp")), "leftover temp files: {names:?}");
}

#[test]
fn atomic_write_replaces_existing_and_open_files() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("genome.db");
    genome_cli::db::write_atomic(&p, b"first").unwrap();
    // A reader still holding the old file open must not block the replace.
    let mut reader = std::fs::File::open(&p).unwrap();
    genome_cli::db::write_atomic(&p, b"second").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"second");
    let mut old = Vec::new();
    reader.read_to_end(&mut old).unwrap();
    assert!(old == b"first" || old == b"second", "{old:?}");
    drop(reader);
    genome_cli::db::write_atomic(&p, b"third").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"third");
    no_temp_files(d.path());
}

#[test]
fn sealed_writer_renames_over_an_existing_store() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("calls.gts");
    let key = Key::random().unwrap();
    for data in [b"old contents".as_slice(), b"new contents, longer than before".as_slice()] {
        let mut w = SealedWriter::create(&p, &key, "t").unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap();
        let mut r = SealedReader::open(&p, &key, "t").unwrap();
        assert_eq!(r.len(), data.len() as u64);
        let mut buf = vec![0u8; data.len()];
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }
    no_temp_files(d.path());
}

#[test]
fn database_lock_excludes_a_second_holder() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("genome.db");
    let held = genome_cli::db::lock_file(&p).unwrap();
    let other = OpenOptions::new().write(true).open(d.path().join("genome.db.lock")).unwrap();
    assert!(matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    drop(held);
    other.try_lock().unwrap();
}

#[test]
fn audit_append_reads_through_its_own_lock() {
    let d = tempfile::tempdir().unwrap();
    for i in 0..3 {
        genome_cli::audit::append(d.path(), None, "test", json!({"i": i})).unwrap();
    }
    let key = Key::random().unwrap();
    for i in 0..3 {
        genome_cli::audit::append(d.path(), Some(&key), "test", json!({"i": i})).unwrap();
    }
    let plain = genome_cli::audit::read(d.path(), None).unwrap();
    assert_eq!(plain.iter().map(|e| e["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
    let sealed = genome_cli::audit::read(d.path(), Some(&key)).unwrap();
    assert_eq!(sealed.len(), 3);
}
