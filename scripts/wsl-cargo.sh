#!/usr/bin/env bash
# Run cargo for solsnipe inside WSL.
#
# Exists because invoking cargo through `wsl.exe -- bash -c "..."` from a Windows
# shell is a quoting minefield: $HOME expands on the Windows side before WSL ever
# sees it, and the inherited Windows PATH contains unescaped parentheses
# ("Program Files (x86)") that break bash parsing outright. A script file has
# none of those problems.
#
# Usage:  wsl -d Ubuntu -- bash scripts/wsl-cargo.sh build --release
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
# target/ lives in the Linux filesystem: that is where build I/O churns, and
# /mnt/c is slow across the 9p bridge. Source stays on Windows so it stays
# editable from both sides.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target/solsnipe}"

cd "$(dirname "$0")/.."
exec cargo "$@"
