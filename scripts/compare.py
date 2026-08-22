#!/usr/bin/env python3
"""Compare two runs, and refuse to let a cost-model change look like an edge.

Removing the double-counted spread from the paper executor moves every P&L
figure by construction: the same trades, priced differently. A side-by-side of
net P&L between a pre-fix and a post-fix run therefore says almost nothing, and
says it very convincingly.

So this reports the cost-model-independent numbers FIRST - win rate, the peak
each position actually reached, how long it survived, what fraction of exits
were forced by a clock rather than a price - and only then the P&L, labelled
with the penalty each run was carrying.

Usage:  python3 scripts/compare.py old.jsonl new.jsonl
"""
import collections
import io
import json
import sys


def load(path):
    rows = []
    for line in io.open(path, encoding="utf-8", errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except ValueError:
            continue
    return rows


def med(xs):
    s = sorted(xs)
    return s[len(s) // 2] if s else 0.0


def summarise(path):
    rows = load(path)
    kind = lambda k: [r["data"] for r in rows if r.get("kind") == k]
    screens = kind("screen")
    buys, sells, closes = kind("fill_buy"), kind("fill_sell"), kind("position_close")
    cands = kind("candidate")

    quoted = {s["mint"]: s["quoted_price_sol"] for s in screens if s.get("quoted_price_sol")}
    gaps = []
    for b in buys:
        q = quoted.get(b["mint"])
        if q:
            gaps.append(((b["price_sol"] / q) - 1.0) * 10_000.0)

    pnls = [c.get("pnl_sol", 0.0) for c in closes]
    wins = [p for p in pnls if p > 0]
    peaks = [c["max_gain_bps"] for c in closes if c.get("max_gain_bps") is not None]
    reasons = collections.Counter(c.get("reason") for c in closes)
    nosell = len([c for c in closes
                  if c["mint"] not in set(s["mint"] for s in sells)])

    return {
        "path": path,
        "candidates": len(cands),
        "entries": len(buys),
        "closed": len(closes),
        "entry_gap_bps": med(gaps) if gaps else None,
        "win_rate": (100.0 * len(wins) / len(pnls)) if pnls else 0.0,
        "wins": len(wins),
        "median_peak_bps": med(peaks) if peaks else None,
        "reached_10pct": (100.0 * sum(1 for p in peaks if p >= 1000) / len(peaks)) if peaks else 0.0,
        "reached_50pct": (100.0 * sum(1 for p in peaks if p >= 5000) / len(peaks)) if peaks else 0.0,
        "max_hold_share": (100.0 * reasons.get("max_hold", 0) / len(closes)) if closes else 0.0,
        "closed_without_selling": nosell,
        "net": sum(pnls),
        "mean": (sum(pnls) / len(pnls)) if pnls else 0.0,
    }


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    a, b = summarise(sys.argv[1]), summarise(sys.argv[2])

    print("")
    print("=" * 78)
    print(" A: %s" % a["path"])
    print(" B: %s" % b["path"])
    print("=" * 78)

    print("")
    print(" THE COST MODEL EACH RUN CARRIED")
    for r in (a, b):
        g = r["entry_gap_bps"]
        print("   %-28s entry fill %s off the screen quote"
              % (r["path"], ("%+.1f bps" % g) if g is not None else "n/a"))
    print("   800 = latency + double-counted spread. 500 = latency only.")
    print("   If these differ, every P&L line below differs BY CONSTRUCTION.")

    print("")
    print(" SELECTION QUALITY - independent of the cost model")
    print(" (these are what a real improvement would move)")
    rows = [
        ("entries", "%d", "entries"),
        ("win rate %", "%.1f", "win_rate"),
        ("median peak bps", "%.0f", "median_peak_bps"),
        ("reached +10%", "%.0f", "reached_10pct"),
        ("reached +50%", "%.0f", "reached_50pct"),
        ("max_hold exits %", "%.0f", "max_hold_share"),
        ("closed w/o selling", "%d", "closed_without_selling"),
    ]
    print("   %-22s %14s %14s" % ("", "A", "B"))
    for label, fmt, key in rows:
        va, vb = a[key], b[key]
        sa = (fmt % va) if va is not None else "n/a"
        sb = (fmt % vb) if vb is not None else "n/a"
        print("   %-22s %14s %14s" % (label, sa, sb))

    print("")
    print(" P&L - read only after the two lines above")
    print("   %-22s %14s %14s" % ("", "A", "B"))
    print("   %-22s %+14.4f %+14.4f" % ("net SOL", a["net"], b["net"]))
    print("   %-22s %+14.4f %+14.4f" % ("mean per trade", a["mean"], b["mean"]))

    if a["entry_gap_bps"] != b["entry_gap_bps"]:
        print("")
        print("   !! The runs used DIFFERENT cost models. The net difference above")
        print("      is mostly arithmetic, not performance. Compare win rate and")
        print("      median peak instead - those do not move when the penalty does.")

    small = [r for r in (a, b) if r["closed"] < 30]
    if small:
        print("")
        print("   !! %s closed fewer than 30 trades. Nothing here separates from"
              % " and ".join(r["path"] for r in small))
        print("      noise at that size.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
