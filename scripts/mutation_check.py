#!/usr/bin/env python3
"""Verify the regression tests actually bite.

A regression test that still passes when you reintroduce the bug is worthless.
This puts each fixed bug back in turn and asserts the matching test fails.

Every entry below corresponds to a bug that was actually shipped in this repo at
some point, most of them found by watching live mainnet traffic rather than by
reading the code.

Run inside WSL:  python3 scripts/mutation_check.py
"""
import io
import os
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
LF = chr(10)

# (label, source file, original, mutated, test that must fail)
MUTATIONS = [
    (
        "take-profit rung denominated in the remainder, not the original size",
        "src/position.rs",
        "let qty = (pos.tokens_initial * (rung.pct / 100.0)).min(pos.tokens_held);",
        "let qty = pos.tokens_held * (rung.pct / 100.0);",
        "take_profit_rung_is_a_share_of_the_original_size",
    ),
    (
        "trailing stop arms on any tick above entry",
        "src/position.rs",
        "let arm_price = pos.entry_price_sol * (1.0 + cfg.trailing_stop_bps as f64 / 10_000.0);",
        "let arm_price = pos.entry_price_sol;",
        "trailing_stop_does_not_arm_just_above_entry",
    ),
    (
        "a failed mark lookup collapses into no-route and force-closes",
        "src/position.rs",
        "            Err(e) => {" + LF
        + "                warn!(mint = %pos.mint, error = %e, \"mark lookup failed - holding\");" + LF
        + "                Mark::Unknown" + LF
        + "            }",
        "            Err(_) => Mark::NoRoute,",
        "a_transport_failure_never_closes_a_position",
    ),
    (
        "a single no-route sweep writes the position off",
        "src/position.rs",
        "const UNROUTABLE_STRIKES: u8 = 3;",
        "const UNROUTABLE_STRIKES: u8 = 1;",
        "three_consecutive_no_route_sweeps_write_the_position_off",
    ),
    (
        "log markers matched anywhere in the stream, not under the emitting program",
        "src/decode.rs",
        "if line.contains(marker) && stack.last() == Some(&program_id) {",
        "if line.contains(marker) {",
        "a_marker_from_a_nested_program_is_not_our_venue",
    ),
    (
        "any transaction touching two non-quote mints is rejected (drops every pool creation)",
        "src/decode.rs",
        "    if held_elsewhere.len() == 1 {",
        "    if false {",
        "picks_the_base_mint_when_an_lp_mint_was_also_created",
    ),
    (
        "Jupiter throttling reported as an absent route instead of an error",
        "src/rpc.rs",
        "        if !status.is_success() {" + LF
        + "            return Err(anyhow!(\"jupiter quote http {status}\"));" + LF
        + "        }",
        "        if !status.is_success() {" + LF
        + "            return Ok(None);" + LF
        + "        }",
        "throttling_is_an_error_not_an_absent_route",
    ),
    (
        "Token-2022 screening back to a denylist, so unknown extensions pass",
        "src/screen/authority.rs",
        "            if BENIGN_EXTENSIONS.contains(&name) {",
        "            if !KNOWN_HOSTILE.iter().any(|(n, _)| *n == name) {",
        "an_unrecognised_extension_is_rejected",
    ),
    (
        "released reservation keeps burning the cooldown window",
        "src/risk.rs",
        "        st.last_entry = st.prev_last_entry;",
        "        let _ = st.prev_last_entry;",
        "releasing_a_reservation_hands_back_the_cooldown",
    ),
    (
        "pool excluded positionally, exempting any whale that outranks the vault",
        "src/screen/holders.rs",
        "        Some(v) if !v.is_empty() => holdings" + LF
        + "            .iter()" + LF
        + "            .filter(|(addr, _)| addr != v)" + LF
        + "            .map(|(_, amt)| *amt)" + LF
        + "            .collect(),",
        "        Some(_) => holdings.iter().skip(1).map(|(_, amt)| *amt).collect(),",
        "a_whale_larger_than_the_vault_is_still_counted",
    ),
    (
        "outstanding LP treated as burned, so a withdrawable pool passes",
        "src/screen/liquidity.rs",
        "        Some(n) => CheckResult::fail(",
        "        Some(n) if n == u128::MAX => CheckResult::fail(",
        "outstanding_lp_is_rejected",
    ),
]


def run_test(name=None):
    env = dict(os.environ)
    env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")
    env.setdefault("CARGO_TARGET_DIR", os.path.expanduser("~/.cargo-target/solsnipe"))
    cmd = ["cargo", "test", "--quiet"]
    if name:
        cmd.append(name)
    r = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True)
    return r.returncode == 0


def read(path):
    return io.open(os.path.join(ROOT, path), encoding="utf-8").read()


def write(path, text):
    io.open(os.path.join(ROOT, path), "w", encoding="utf-8", newline=LF).write(text)


originals = {}
for _, path, _, _, _ in MUTATIONS:
    if path not in originals:
        originals[path] = read(path)

weak = []
try:
    for label, path, old, new, test in MUTATIONS:
        source = originals[path]
        if old not in source:
            print("SKIP  " + label)
            print("      anchor not found in " + path + " - the fix may have been rewritten")
            weak.append(label)
            continue
        write(path, source.replace(old, new, 1))
        if run_test(test):
            print("WEAK  " + label)
            print("      test " + test + " STILL PASSES against the bug")
            weak.append(label)
        else:
            print("GOOD  " + label)
            print("      test " + test + " correctly fails")
        write(path, source)
finally:
    for path, source in originals.items():
        write(path, source)

print("")
print("sources restored; full suite green: " + str(run_test()))
print(str(len(MUTATIONS) - len(weak)) + "/" + str(len(MUTATIONS)) + " mutations caught")
sys.exit(1 if weak else 0)
