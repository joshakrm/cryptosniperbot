#!/usr/bin/env bash
# Smoke-test the solsnipe CLI and, more importantly, its refusals.
# The point of a fail-closed design is that it actually refuses, so assert that.
set -uo pipefail

BIN="${1:?usage: smoke.sh /path/to/solsnipe}"
EX="$(cd "$(dirname "$0")/.." && pwd)/config.example.toml"
DIR="$(mktemp -d)"
MINT="So11111111111111111111111111111111111111112"
trap 'rm -rf "$DIR"' EXIT

sep() { echo; echo "=== $* ==="; }

sep "1. help"
"$BIN" --help 2>&1 | head -14

sep "2. stats on a missing journal (expect a clean error, not a panic)"
"$BIN" --journal "$DIR/nope.jsonl" stats 2>&1 | tail -3

sep "3. config still has the YOUR_KEY placeholder (expect refusal)"
cp "$EX" "$DIR/placeholder.toml"
"$BIN" --config "$DIR/placeholder.toml" screen "$MINT" 2>&1 | tail -3

sep "4. screen.min_holders = 50, above the 20 the RPC can return (expect refusal)"
sed -e "s|YOUR_KEY|dummy|g" -e "s|^min_holders.*|min_holders = 50|" "$EX" > "$DIR/holders.toml"
"$BIN" --config "$DIR/holders.toml" screen "$MINT" 2>&1 | tail -3

sep "5. take_profit rungs sum to 130% of the position (expect refusal)"
sed -e "s|YOUR_KEY|dummy|g"     -e "s|{ gain_bps = 5000,  pct = 50.0 }|{ gain_bps = 5000,  pct = 80.0 }|"     "$EX" > "$DIR/tp.toml"
"$BIN" --config "$DIR/tp.toml" screen "$MINT" 2>&1 | tail -3

sep "6. live.enabled = true (expect refusal to start, not silent pretending)"
sed -e "s|YOUR_KEY|dummy|g" -e "s|^enabled = false|enabled = true|" "$EX" > "$DIR/live.toml"
"$BIN" --config "$DIR/live.toml" run 2>&1 | tail -3

sep "7. position_size_sol = 0 (expect refusal)"
sed -e "s|YOUR_KEY|dummy|g" -e "s|^position_size_sol.*|position_size_sol = 0.0|" "$EX" > "$DIR/size.toml"
"$BIN" --config "$DIR/size.toml" screen "$MINT" 2>&1 | tail -3

echo
echo "=== smoke complete ==="
