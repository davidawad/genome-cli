#!/usr/bin/env bash
# Printed env is eval'd by the land quality gate (dotfiles done-worktree-qualitygate.sh)
# so its clippy/llvm-cov runs reuse this repo's warm target dir, matching the justfile.
echo "export CARGO_TARGET_DIR=\"${CARGO_TARGET_DIR:-$HOME/.cache/targets/genome-cli}\""
