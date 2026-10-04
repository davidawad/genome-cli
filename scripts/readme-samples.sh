#!/usr/bin/env bash
# Regenerate the README sample sessions and the hero screenshot from scratch.
#
#   scripts/readme-samples.sh           # rewrite docs/samples/*.txt, docs/screenshots/*.svg, README.md
#   scripts/readme-samples.sh --check   # fail if any of them differ from what the binary prints now
#
# Runs the real `genome` binary ($GENOME_BIN, else a fresh `cargo build --release`)
# from the repo root against the synthetic fixtures in tests/fixtures, in a
# throwaway data dir. The fixture chain files stand in for the UCSC ones so the
# session runs offline; SOURCE_DATE_EPOCH pins the JSON `generated_at`.
# GENOME_KEY is a throwaway passphrase for the (encrypted) sample database;
# GENOME_INSECURE_FAST_KDF=1 weakens Argon2id for speed and is for tests only.
# Needs bash and python3.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

check=false
case "${1:-}" in
    --check) check=true ;;
    "") ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

if [[ -z "${GENOME_BIN:-}" ]]; then
    cargo build --release --quiet
    GENOME_BIN="${CARGO_TARGET_DIR:-$root/target}/release/genome"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/cache/chains" "$work/out/samples" "$work/out/screenshots"
cp tests/fixtures/hg19ToHg38.chain "$work/cache/chains/hg19ToHg38.over.chain.gz"
cp tests/fixtures/hg38ToHg19.chain "$work/cache/chains/hg38ToHg19.over.chain.gz"

# One prompt line plus the command's combined stdout/stderr, as a terminal shows it.
genome() {
    printf '$ genome %s\n' "$*"
    env -i PATH="$PATH" HOME="$work" XDG_CONFIG_HOME="$work/config" \
        GENOME_DATA_DIR="$work/data" GENOME_CACHE_DIR="$work/cache" GENOME_OFFLINE=1 \
        GENOME_COLOR=always SOURCE_DATE_EPOCH=1790000000 \
        GENOME_KEY=readme-sample-key GENOME_INSECURE_FAST_KDF=1 \
        "$GENOME_BIN" "$@" 2>&1
}

{
    genome import tests/fixtures/23andme_male.txt --name jane
    genome import tests/fixtures/wgs_grch38.vcf.gz --name wgs
    genome summary
    genome lookup wgs --rsid rs429358,rs7412
    genome compare wgs jane
} >"$work/out/tour.ansi"

{
    genome lookup wgs --rsid rs7412 --format json
} >"$work/out/json.ansi"

{
    genome pipeline plan SYN_S1_L001_R1_001.fastq.gz SYN_S1_L001_R2_001.fastq.gz \
        --out run1 --region chr19:44.9M-45.0M --quiet --columns step,tool,status
} >"$work/out/pipeline.ansi"

for s in tour json pipeline; do
    sed $'s/\x1b\\[[0-9;]*m//g' "$work/out/$s.ansi" >"$work/out/samples/$s.txt"
done
python3 scripts/ansi2svg.py --title 'genome: 60-second tour (synthetic data)' \
    "$work/out/tour.ansi" "$work/out/screenshots/tour.svg"
cp README.md "$work/out/README.md"
python3 - "$work/out" <<'PY'
import pathlib, re, sys

out = pathlib.Path(sys.argv[1])
readme = out / "README.md"
text = readme.read_text()
for sample in sorted((out / "samples").glob("*.txt")):
    name = sample.stem
    block = f"<!-- sample:{name} -->\n```console\n{sample.read_text()}```\n<!-- /sample:{name} -->"
    pattern = re.compile(rf"<!-- sample:{name} -->.*?<!-- /sample:{name} -->", re.S)
    if not pattern.search(text):
        sys.exit(f"README.md has no <!-- sample:{name} --> ... <!-- /sample:{name} --> block")
    text = pattern.sub(lambda _: block, text)
readme.write_text(text)
PY

targets=(docs/samples/tour.txt docs/samples/json.txt docs/samples/pipeline.txt docs/screenshots/tour.svg README.md)
generated() {
    case "$1" in
        README.md) echo "$work/out/README.md" ;;
        *) echo "$work/out/${1#docs/}" ;;
    esac
}

if $check; then
    status=0
    for t in "${targets[@]}"; do
        if ! diff -u "$t" "$(generated "$t")"; then
            echo "readme-samples: $t is stale; run scripts/readme-samples.sh" >&2
            status=1
        fi
    done
    exit $status
fi

mkdir -p docs/samples docs/screenshots
for t in "${targets[@]}"; do
    cp "$(generated "$t")" "$t"
done
echo "readme-samples: wrote ${targets[*]}"
