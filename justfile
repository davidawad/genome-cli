# Gate run by the land queue before merging.
test-gate:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Release build of the `genome` binary.
build:
    cargo build --release

# End-to-end FASTQ pipeline test (needs minimap2, samtools, bcftools on PATH).
pipeline-e2e:
    cargo test --test pipeline_e2e -- --nocapture
