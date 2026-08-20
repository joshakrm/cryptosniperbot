#!/usr/bin/env bash
# Start solsnipe detached for a long run, logging somewhere persistent.
#
# Two traps this avoids:
#   - WSL /tmp is tmpfs and is wiped when the distro idles out, taking the log
#     with it. The log goes next to the source instead (gitignored).
#   - Passing shell variables through PowerShell -> wsl.exe -> bash mangles them,
#     so everything here is a literal path.
#
# Usage:  wsl -d Ubuntu -- bash scripts/run-long.sh [seconds]
set -uo pipefail

DIR=/mnt/c/Users/joshr/Documents/solsnipe
BIN=/home/josh/.cargo-target/solsnipe/release/solsnipe
SECS="${1:-3600}"
CFG="${2:-config.toml}"
TAG="${3:-}"

cd "$DIR" || exit 1
JOURNAL="$DIR/journal${TAG}.jsonl"
LOG="$DIR/run${TAG}.log"
rm -f "$JOURNAL" "$LOG"

nohup env RUST_LOG=solsnipe=info timeout "$SECS" "$BIN" --config "$DIR/$CFG" --journal "$JOURNAL" run > "$LOG" 2>&1 &
PID=$!
sleep 10

echo "started pid $PID for ${SECS}s"
echo "config:  $CFG"
echo "log:     $LOG"
echo "journal: $JOURNAL"
echo "--- first output ---"
tail -6 "$LOG"
