# Share one warm target dir across this repo's worktrees (each land runs in a
# fresh worktree; a per-worktree target/ means a cold build every time).
export CARGO_TARGET_DIR := env("CARGO_TARGET_DIR", home_directory() / ".cache" / "targets" / "genome-cli")

# Gate run by the land queue before merging.
test-gate:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Regenerate README sample sessions (docs/samples) and the hero screenshot (docs/screenshots).
readme:
    scripts/readme-samples.sh

# Release build of the `genome` binary.
build:
    cargo build --release

# End-to-end FASTQ pipeline test (needs minimap2, samtools, bcftools on PATH).
pipeline-e2e:
    cargo test --test pipeline_e2e -- --nocapture

# Coverage report (slow, instrumented build); not part of test-gate.
coverage:
    cargo llvm-cov --html
