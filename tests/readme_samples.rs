//! Golden check: the README's sample sessions, `docs/samples/*.txt` and the hero
//! screenshot must match what the binary prints now. Regenerate with
//! `scripts/readme-samples.sh` (`just readme`).

use std::path::Path;
use std::process::Command;

#[test]
fn readme_samples_match_binary_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash")
        .arg(root.join("scripts/readme-samples.sh"))
        .arg("--check")
        .env("GENOME_BIN", env!("CARGO_BIN_EXE_genome"))
        .current_dir(root)
        .output()
        .expect("run scripts/readme-samples.sh (needs bash and python3)");
    assert!(
        out.status.success(),
        "README samples drifted from the binary's output; run `just readme` and commit.\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
