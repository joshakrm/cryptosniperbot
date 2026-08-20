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

cd "$DIR" || exit 1
rm -f "$DIR/journal.jsonl" "$DIR/run.log"

nohup env RUST_LOG=solsnipe=info timeout "$SECS" "$BIN" run > "$DIR/run.log" 2>&1 &
PID=$!
sleep 10

echo "started pid $PID for ${SECS}s"
echo "log:     $DIR/run.log"
echo "journal: $DIR/journal.jsonl"
echo "--- first output ---"
tail -6 "$DIR/run.log"
