#!/usr/bin/env bash
# Start solsnipe in the FOREGROUND, so the caller's WSL session stays attached.
#
# This exists because run-long.sh silently loses long runs.
#
# WSL2 shuts the entire VM down shortly after the last client session detaches
# (vmIdleTimeout, 60s by default). run-long.sh nohups the bot and returns, so the
# launching wsl.exe exits and the VM goes down about a minute later - killing the
# bot with it. Nothing appears in the log: no panic, no error, the output just
# stops mid-run, which reads exactly like a bot that stopped finding trades.
#
# It went unnoticed because polling the run kept it alive. Every `wsl` command
# re-attaches a session and resets the idle timer, so a run being watched
# survives and a run left alone does not. A 119-minute run completed while being
# checked on; the next one died after 25 seconds when it was not.
#
# So this script does NOT detach. The caller holds the session for the full
# duration, and the run lives exactly as long as it is supposed to.
#
# Usage:  wsl -e bash -lc 'bash /mnt/c/.../scripts/run-attached.sh 10800 config-free.toml -free'
#
# For a run that must outlive every session, set vmIdleTimeout instead - in
# C:\Users\<you>\.wslconfig, [wsl2] / vmIdleTimeout = -1, then `wsl --shutdown`.
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

echo "solsnipe attached run: ${SECS}s, config $CFG"
echo "journal: $JOURNAL"
echo "log:     $LOG"

# exec so signals reach the bot directly rather than a wrapping shell.
exec env RUST_LOG=solsnipe=info timeout "$SECS" \
    "$BIN" --config "$DIR/$CFG" --journal "$JOURNAL" run > "$LOG" 2>&1
