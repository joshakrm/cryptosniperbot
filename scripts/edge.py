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

TWO COUNTS, ALWAYS. Most shadowed candidates never become routable at all, so
they carry no price and no peak. An earlier version of this script printed the
group size next to percentages computed only over the priced subset - showing
"n=78, reached +10% 50%" when eight records were priced and four drove the
headline. Every row here now prints priced/total, and a comparison is withheld
entirely below MIN_COMPARE priced records rather than displayed with a caveat.
Caveats get skimmed; absence does not.

Never-routable is itself reported, because it is an outcome and currently the
majority one: a candidate that never became tradeable was never an opportunity.

Usage:  python3 scripts/edge.py [journal.jsonl ...]
"""
import collections
import glob
import io
import json
import sys

# Below this many priced records on a side, a comparison is not shown at all.
MIN_COMPARE = 30


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


def peaks_of(group):
    return [g["peak_ret"] for g in group if g.get("peak_ret") is not None]


def summarise(label, group, base_peaks=None):
    """Print one row. n is always priced/total so the two can never be confused."""
    total = len(group)
    if not total:
        print("   %-24s  -" % label)
        return []
    peaks = peaks_of(group)
    nr = sum(1 for g in group if g.get("never_routable"))
    head = "   %-24s  priced %3d/%-4d  never-routable %3d (%3.0f%%)" % (
        label, len(peaks), total, nr, 100.0 * nr / total)
    if not peaks:
        print(head + "   no priced marks")
        return []
    up10 = 100.0 * sum(1 for p in peaks if p >= 0.10) / len(peaks)
    up50 = 100.0 * sum(1 for p in peaks if p >= 0.50) / len(peaks)
    print(head)
    print("        of the priced: median peak %+6.1f%%   reached +10%% %3.0f%%   +50%% %3.0f%%"
          % (100.0 * med(peaks), up10, up50))

    if base_peaks is not None:
        if len(peaks) < MIN_COMPARE or len(base_peaks) < MIN_COMPARE:
            print("        (not compared: needs %d priced per side, have %d vs %d)"
                  % (MIN_COMPARE, len(peaks), len(base_peaks)))
        else:
            base_up10 = 100.0 * sum(1 for p in base_peaks if p >= 0.10) / len(base_peaks)
            delta = up10 - base_up10
            if delta > 5.0:
                print("        ^^ these REJECTED candidates beat the approvals by %.0fpp on" % delta)
                print("           reaching +10%. That filter is removing winners.")
    return peaks


def main():
    paths = sys.argv[1:] or sorted(glob.glob("*.jsonl"))
    shadows = load_shadows(paths)
    if not shadows:
        print("No shadow records found.")
        print("Enable it in the config ([shadow] enabled = true) and let the bot run;")
        print("each shadowed candidate takes about 15 minutes to complete.")
        return 1

    priced_total = len(peaks_of(shadows))
    print("")
    print("=" * 78)
    print(" edge analysis: %d shadowed candidates, %d of them priced"
          % (len(shadows), priced_total))
    print("=" * 78)
    print("")
    print(" Read the priced count on every row, not the total. Percentages are")
    print(" computed over priced records only, and most candidates never price.")

    approved = [s for s in shadows if s.get("approved")]
    rejected = [s for s in shadows if not s.get("approved")]

    print("")
    print(" BASELINE")
    base_peaks = summarise("approved (traded)", approved)
    summarise("rejected (all)", rejected)

    print("")
    print(" BY REJECTION REASON")
    print(" (a reason whose rejects OUTPERFORM the approvals is costing you money)")
    by_reason = collections.defaultdict(list)
    for s in rejected:
        for reason in s.get("rejected_by") or ["<none>"]:
            by_reason[reason].append(s)
    for reason, group in sorted(by_reason.items(), key=lambda kv: -len(kv[1])):
        summarise(reason, group, base_peaks=base_peaks)

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
    for band in ["< 1 SOL", "1-5 SOL", "5-20 SOL", "20-100 SOL", "100+ SOL"]:
        if banded.get(band):
            summarise(band, banded[band])

    print("")
    print(" NEVER-ROUTABLE IS AN OUTCOME, NOT A GAP")
    nr_a = sum(1 for s in approved if s.get("never_routable"))
    nr_r = sum(1 for s in rejected if s.get("never_routable"))
    print("   approved: %3d of %3d never became routable (%3.0f%%)"
          % (nr_a, len(approved), 100.0 * nr_a / max(1, len(approved))))
    print("   rejected: %3d of %3d never became routable (%3.0f%%)"
          % (nr_r, len(rejected), 100.0 * nr_r / max(1, len(rejected))))

    print("")
    if priced_total < 2 * MIN_COMPARE:
        print(" %d priced records is not enough for any comparison above." % priced_total)
        print(" A 10pp difference on a low base rate needs roughly 400 per group.")
        print(" Keep collecting. Nothing here should change the config yet.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
