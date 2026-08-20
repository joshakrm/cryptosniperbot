#!/usr/bin/env python3
"""Turn a solsnipe journal into a report you can actually act on.

`solsnipe stats` is deliberately minimal. This digs further: the funnel from
detection to fill, the PnL distribution rather than just its mean, hold times,
and a per-venue split.

The most important thing it does is refuse to flatter you. Open positions are
excluded from PnL but reported separately, because losers resolve in seconds
while winners take minutes - so any mid-run PnL is drawn from the wrong end of
the distribution and looks far worse than reality.

Usage:  python3 scripts/report.py [path/to/journal.jsonl]
"""
import collections
import io
import json
import os
import sys

LF = chr(10)


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


def bar(n, total, width=28):
    if total <= 0:
        return ""
    return "#" * max(0, int(round(width * n / float(total))))


def pct(n, d):
    return (100.0 * n / d) if d else 0.0


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "journal.jsonl"
    if not os.path.exists(path):
        print("no journal at " + path)
        return 1

    rows = load(path)
    if not rows:
        print("journal is empty")
        return 1

    kinds = collections.Counter(r.get("kind") for r in rows)
    screens = [r["data"] for r in rows if r.get("kind") == "screen"]
    cands = [r["data"] for r in rows if r.get("kind") == "candidate"]
    buys = [r["data"] for r in rows if r.get("kind") == "fill_buy"]
    sells = [r["data"] for r in rows if r.get("kind") == "fill_sell"]
    closes = [r["data"] for r in rows if r.get("kind") == "position_close"]
    opens = [r["data"] for r in rows if r.get("kind") == "position_open"]
    blocks = [r["data"] for r in rows if r.get("kind") == "risk_block"]

    def blocking(s):
        return [
            c
            for c in s.get("checks", [])
            if not c.get("passed") and c.get("severity") != "advisory"
        ]

    approved = [s for s in screens if not blocking(s)]

    print("")
    print("=" * 66)
    print(" solsnipe report: " + path)
    print("=" * 66)

    # ---- funnel ---------------------------------------------------------
    print("")
    print(" FUNNEL")
    stages = [
        ("launches detected", len(cands)),
        ("screened", len(screens)),
        ("approved", len(approved)),
        ("entered", len(buys)),
        ("closed", len(closes)),
    ]
    top = stages[0][1] or 1
    for label, n in stages:
        print("   %-20s %5d  %5.1f%%  %s" % (label, n, pct(n, top), bar(n, top)))

    still_open = len(opens) - len(closes)
    if still_open > 0:
        print("")
        print("   %d position(s) STILL OPEN and excluded from PnL below." % still_open)
        print("   Losers hit their stop in seconds; winners need minutes to reach a")
        print("   take-profit rung. Mid-run PnL is therefore biased pessimistic.")

    # ---- why candidates died -------------------------------------------
    fatal = collections.Counter()
    unavail = collections.Counter()
    for s in screens:
        for c in blocking(s):
            if c.get("severity") == "unavailable":
                unavail[c["name"]] += 1
            else:
                fatal[c["name"]] += 1

    if fatal:
        print("")
        print(" REJECTED (the screening working)")
        for name, n in fatal.most_common():
            print("   %-18s %5d  %s" % (name, n, bar(n, len(screens))))
    if unavail:
        print("")
        print(" COULD NOT CHECK (your endpoint, not the tokens)")
        for name, n in unavail.most_common():
            print("   %-18s %5d  %s" % (name, n, bar(n, len(screens))))
        tot = sum(unavail.values())
        if tot >= len(screens) * 0.15:
            print("   -> %.0f%% of screens were degraded. Fix the endpoint before" % pct(tot, len(screens)))
            print("      drawing any conclusion from the numbers below.")

    if blocks:
        print("")
        print(" APPROVED BUT NOT TAKEN (risk gates)")
        for reason, n in collections.Counter(
            b.get("reason", "?").split(":")[0] for b in blocks
        ).most_common():
            print("   %-30s %4d" % (reason, n))

    # ---- venue split ----------------------------------------------------
    if cands:
        print("")
        print(" BY VENUE")
        byv = collections.Counter(c.get("venue", "?") for c in cands)
        entered_mints = set(b["mint"] for b in buys)
        vmints = collections.defaultdict(set)
        for c in cands:
            vmints[c.get("venue", "?")].add(c.get("mint"))
        for v, n in byv.most_common():
            ent = len(vmints[v] & entered_mints)
            print("   %-16s %4d detected   %3d entered" % (v, n, ent))

    # ---- pnl ------------------------------------------------------------
    if closes:
        pnls = sorted(c.get("pnl_sol", 0.0) for c in closes)
        wins = [p for p in pnls if p > 0]
        losses = [p for p in pnls if p <= 0]
        total = sum(pnls)
        print("")
        print(" CLOSED TRADES (%d)" % len(pnls))
        print("   net PnL          %+.4f SOL" % total)
        print("   win rate         %.1f%%  (%d win / %d loss)" % (pct(len(wins), len(pnls)), len(wins), len(losses)))
        print("   average          %+.4f SOL" % (total / len(pnls)))
        print("   best             %+.4f SOL" % pnls[-1])
        print("   worst            %+.4f SOL" % pnls[0])
        if wins:
            print("   avg win          %+.4f SOL" % (sum(wins) / len(wins)))
        if losses:
            print("   avg loss         %+.4f SOL" % (sum(losses) / len(losses)))
        if wins and losses:
            edge = (sum(wins) / len(wins)) / abs(sum(losses) / len(losses))
            print("   win/loss ratio   %.2fx" % edge)
            print("   -> sniping is normally low-win-rate and fat-tailed. A small")
            print("      win rate with a large ratio is a working strategy; a high")
            print("      win rate with a ratio under 1 means the losers are too big.")

        print("")
        print(" EXIT REASONS")
        for reason, n in collections.Counter(c.get("reason", "?") for c in closes).most_common():
            print("   %-16s %4d  %s" % (reason, n, bar(n, len(closes))))
    else:
        print("")
        print(" No positions have closed yet - nothing to say about PnL.")

    if sells:
        print("")
        print(" PARTIAL EXITS: %d (take-profit rungs firing before full close)" % len(sells))

    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
