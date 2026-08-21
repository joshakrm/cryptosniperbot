#!/usr/bin/env python3
"""Ask whether the filters remove losers or remove winners.

Every P&L figure in this project describes tokens that PASSED screening, which
cannot distinguish a filter that saves money from one that costs it - both look
like a high rejection count. The shadow tracker fixes that by following a random
sample of candidates regardless of the verdict, so rejected candidates have
outcomes too.

This compares them. For each rejection reason, it shows how the candidates that
reason rejected actually performed against the ones that were approved. A reason
whose rejects outperform the approvals is destroying money, however sensible it
sounds.

Usage:  python3 scripts/edge.py [journal.jsonl ...]
"""
import collections
import glob
import io
import json
import sys


def load_shadows(paths):
    out = []
    for path in paths:
        try:
            fh = io.open(path, encoding="utf-8", errors="replace")
        except IOError:
            continue
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("kind") == "shadow":
                out.append(r["data"])
    return out


def med(xs):
    s = sorted(xs)
    return s[len(s) // 2] if s else 0.0


def summarise(label, group, base=None):
    if not group:
        print("   %-24s  n=0" % label)
        return
    peaks = [g.get("peak_ret") for g in group if g.get("peak_ret") is not None]
    finals = []
    for g in group:
        marks = [m for m in (g.get("marks") or []) if m.get("ret") is not None]
        if marks:
            finals.append(marks[-1]["ret"])
    if not peaks:
        print("   %-24s  n=%-4d  no priced marks" % (label, len(group)))
        return
    up10 = 100.0 * sum(1 for p in peaks if p >= 0.10) / len(peaks)
    up50 = 100.0 * sum(1 for p in peaks if p >= 0.50) / len(peaks)
    line = "   %-24s  n=%-4d  median peak %+6.1f%%  reached +10%% %4.0f%%  +50%% %4.0f%%" % (
        label, len(group), 100.0 * med(peaks), up10, up50)
    if finals:
        line += "  median final %+6.1f%%" % (100.0 * med(finals))
    print(line)
    if base is not None and base:
        base_up10 = 100.0 * sum(1 for p in base if p >= 0.10) / len(base)
        delta = up10 - base_up10
        if delta > 5.0:
            print("        ^^ these REJECTED candidates beat the approvals by %.0fpp on reaching +10%%." % delta)
            print("           That filter is removing winners." )
    return peaks


def main():
    paths = sys.argv[1:] or sorted(glob.glob("*.jsonl"))
    shadows = load_shadows(paths)
    if not shadows:
        print("No shadow records found.")
        print("Enable it in the config ([shadow] enabled = true) and let the bot run;")
        print("each shadowed candidate takes about 15 minutes to complete.")
        return 1

    print("")
    print("=" * 78)
    print(" edge analysis: %d shadowed candidates" % len(shadows))
    print("=" * 78)

    approved = [s for s in shadows if s.get("approved")]
    rejected = [s for s in shadows if not s.get("approved")]

    print("")
    print(" BASELINE")
    base_peaks = summarise("approved (traded)", approved) or []
    summarise("rejected (all)", rejected)

    print("")
    print(" BY REJECTION REASON")
    print(" (a reason whose rejects OUTPERFORM the approvals is costing you money)")
    by_reason = collections.defaultdict(list)
    for s in rejected:
        for reason in s.get("rejected_by") or ["<none>"]:
            by_reason[reason].append(s)
    for reason, group in sorted(by_reason.items(), key=lambda kv: -len(kv[1])):
        summarise(reason, group, base=base_peaks)

    print("")
    print(" BY VENUE")
    by_venue = collections.defaultdict(list)
    for s in shadows:
        by_venue[s.get("venue") or "?"].append(s)
    for venue, group in sorted(by_venue.items(), key=lambda kv: -len(kv[1])):
        summarise(venue, group)

    print("")
    print(" DOES POOL SIZE PREDICT ANYTHING?")
    banded = collections.defaultdict(list)
    for s in shadows:
        p = s.get("pool_sol")
        if p is None:
            continue
        band = ("< 1 SOL" if p < 1 else "1-5 SOL" if p < 5 else
                "5-20 SOL" if p < 20 else "20-100 SOL" if p < 100 else "100+ SOL")
        banded[band].append(s)
    order = ["< 1 SOL", "1-5 SOL", "5-20 SOL", "20-100 SOL", "100+ SOL"]
    for band in order:
        if banded.get(band):
            summarise(band, banded[band])

    print("")
    n = len(shadows)
    if n < 200:
        print(" NOTE: %d samples is not enough to trust any difference above." % n)
        print(" A 10pp difference in a 5%%-base rate needs roughly 400 per group to")
        print(" separate from noise. Keep collecting before acting on this.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
