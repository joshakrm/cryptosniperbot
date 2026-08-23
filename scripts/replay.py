#!/usr/bin/env python3
"""Replay recorded price paths through different exit policies.

Every mark the bot takes is journalled with the position age and the gain
against the fill price, so a position's whole price path is on disk. That means
take-profit ladders, stops and hold times can be compared on data already
collected, without running the bot again for each guess.

TWO THINGS MAKE THIS EASY TO GET WRONG, AND BOTH ARE HANDLED HERE.

Truncation. A position that stopped out at -35% has no recorded prices after
the stop, so any looser stop replayed against it is scored on a path that ends
exactly where the old rule cut it. That biases every comparison toward tighter
rules - the tighter rule is the only one whose data is complete. So the default
is to use ONLY positions whose path ran to the hold limit, where nothing was cut
short. Pass --all to include truncated paths and see how much the bias moves it.

Overfitting. Searching a grid of policies against one dataset finds the policy
that best fits that dataset's noise. Paths are therefore split by time: policies
are ranked on the earlier half and reported on the later half, which they had no
part in choosing. The out-of-sample column is the only one worth reading.

AND THE SPLIT DOES NOT PROTECT YOU FROM A CHANGE OF DISTRIBUTION, which is how
this script has already been wrong once. It ranked a +10% take-profit best
out-of-sample, -11.15% against -15.38% for +50%. Shipped, it raised the live win
rate 9.1% -> 13.6% and collapsed the win/loss ratio 5.63x -> 0.91x, leaving the
mean per trade identical. The paths it learned from were recorded under
min_pool_sol = 2.0; the config it was applied to screened at 5.0 and saw a
different candidate mix entirely. A time split defends against fitting noise
WITHIN a distribution and says nothing about being carried into another one.

So: re-record paths under the config you actually intend to run, and re-fit on
those. The script now prints which journals it read and warns when they span
more than one screening config, but it cannot detect every such change - that
judgement stays with you.

Usage:  python3 scripts/replay.py [--all] [journal.jsonl ...]
"""
import glob
import io
import json
import sys

EXIT_COST_BPS = 250.0     # half the latency penalty, matching src/exec/paper.rs


def load_paths(paths):
    """mint -> (first_ts, [(age_s, gain_bps), ...]) sorted by age."""
    marks = {}
    first = {}
    for p in paths:
        try:
            fh = io.open(p, encoding="utf-8", errors="replace")
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
            if r.get("kind") != "mark":
                continue
            d = r["data"]
            m = d.get("mint")
            if m is None or d.get("gain_bps") is None:
                continue
            marks.setdefault(m, []).append((d.get("age_s", 0), d["gain_bps"]))
            if m not in first:
                first[m] = r.get("ts", "")
    out = {}
    for m, pts in marks.items():
        pts.sort()
        if len(pts) >= 2:
            out[m] = (first[m], pts)
    return out


def simulate(path, tp, sl_bps, trail_bps, max_hold):
    """Return the net multiple of a position under one policy, minus 1.

    tp is [(gain_bps, pct_of_original)]. Selling costs EXIT_COST_BPS, matching
    what the paper executor charges, so policies are compared on what they would
    actually have netted rather than on quoted prices.
    """
    cost = 1.0 - EXIT_COST_BPS / 10_000.0
    remaining = 100.0
    realised = 0.0
    peak = None
    rung = 0
    last = path[0][1]

    for age, gain in path:
        last = gain
        peak = gain if peak is None else max(peak, gain)

        while rung < len(tp) and gain >= tp[rung][0] and remaining > 0:
            sell = min(tp[rung][1], remaining)
            realised += (sell / 100.0) * (1.0 + gain / 10_000.0) * cost
            remaining -= sell
            rung += 1
        if remaining <= 0:
            return realised - 1.0

        if trail_bps and peak is not None and gain <= peak - trail_bps:
            realised += (remaining / 100.0) * (1.0 + gain / 10_000.0) * cost
            return realised - 1.0

        if gain <= -sl_bps:
            realised += (remaining / 100.0) * (1.0 + gain / 10_000.0) * cost
            return realised - 1.0

        if age >= max_hold:
            realised += (remaining / 100.0) * (1.0 + gain / 10_000.0) * cost
            return realised - 1.0

    realised += (remaining / 100.0) * (1.0 + last / 10_000.0) * cost
    return realised - 1.0


def score(paths, policy):
    rets = [simulate(p, policy["tp"], policy["sl"], policy["trail"], policy["hold"])
            for p in paths]
    if not rets:
        return None
    wins = [r for r in rets if r > 0]
    return {
        "n": len(rets),
        "mean": sum(rets) / len(rets),
        "total": sum(rets),
        "win_rate": 100.0 * len(wins) / len(rets),
        "best": max(rets),
    }


def policies():
    """A deliberately coarse grid. A fine one would fit noise better and mean less."""
    out = []
    # Measured: of 28 positions that reached +50%, the median peak is +152% and
    # exactly half ever touch the current second rung at +150%. A rung at +100%
    # fills on 61% and at +75% on 75%, so the intermediate levels are where the
    # question actually lives - the original grid jumped straight past them.
    ladders = [
        ("current 50@+50/50@+150", [(5000, 50.0), (15000, 50.0)]),
        ("50@+50 / 50@+125", [(5000, 50.0), (12500, 50.0)]),
        ("50@+50 / 50@+100", [(5000, 50.0), (10000, 50.0)]),
        ("50@+50 / 50@+75", [(5000, 50.0), (7500, 50.0)]),
        ("70@+50 / 30@+150", [(5000, 70.0), (15000, 30.0)]),
        ("30@+50 / 70@+100", [(5000, 30.0), (10000, 70.0)]),
        ("half at +25, half at +100", [(2500, 50.0), (10000, 50.0)]),
        ("all at +50", [(5000, 100.0)]),
        ("all at +75", [(7500, 100.0)]),
        ("all at +100", [(10000, 100.0)]),
        ("thirds +25/+75/+200", [(2500, 33.0), (7500, 33.0), (20000, 34.0)]),
        ("thirds +50/+100/+200", [(5000, 33.0), (10000, 33.0), (20000, 34.0)]),
        ("none - ride to stop or clock", []),
    ]
    for sl in (2000, 3500, 5000, 9000):
        for trail in (0, 2500, 4000):
            for hold in (900, 1800):
                for name, tp in ladders:
                    out.append({"name": "%s | sl%d trail%d hold%ds"
                                        % (name, sl, trail, hold),
                                "tp": tp, "sl": sl, "trail": trail, "hold": hold})
    return out


def main():
    argv = [a for a in sys.argv[1:] if a != "--all"]
    use_all = "--all" in sys.argv
    files = argv or sorted(glob.glob("journal*.jsonl"))

    paths = load_paths(files)
    if not paths:
        print("No mark records found. Run the bot with journalling first.")
        return 1

    # Name the sources. A policy fitted across journals recorded under different
    # screening configs is fitted across different populations, and that has
    # already produced one wrong shipped answer.
    print("")
    print(" reading: %s" % ", ".join(files))
    if len(files) > 1:
        print(" NOTE: multiple journals. If they were recorded under different")
        print(" screening configs - a different min_pool_sol, a different venue")
        print(" mix - they are different populations and a policy fitted across")
        print(" them describes none of them. Re-record under one config first.")

    complete = {m: v for m, v in paths.items() if v[1][-1][0] >= 870}
    chosen = paths if use_all else complete

    print("")
    print("=" * 78)
    print(" exit-policy replay")
    print("=" * 78)
    print(" positions with a recorded path : %d" % len(paths))
    print(" of those, ran to the hold limit: %d" % len(complete))
    if use_all:
        print("")
        print(" --all: TRUNCATED PATHS INCLUDED. Positions stopped out early have no")
        print(" prices after their stop, so looser rules are scored on paths that end")
        print(" where the old rule cut them. Every comparison below is biased toward")
        print(" tighter rules. This is the number to distrust.")
    else:
        print(" using only complete paths, where no rule truncated the data")

    if len(chosen) < 20:
        print("")
        print(" %d usable paths is too few to choose a policy from." % len(chosen))
        print(" Collect more before acting on anything below.")
        if not chosen:
            return 1

    ordered = sorted(chosen.items(), key=lambda kv: kv[1][0])
    half = len(ordered) // 2
    train = [v[1] for _, v in ordered[:half]]
    test = [v[1] for _, v in ordered[half:]]
    print(" split by time: %d earlier (fit) / %d later (report)" % (len(train), len(test)))

    scored = []
    for pol in policies():
        tr = score(train, pol)
        te = score(test, pol)
        if tr and te:
            scored.append((tr["mean"], pol, tr, te))
    scored.sort(key=lambda x: -x[0])

    print("")
    print(" RANKED ON THE EARLIER HALF, REPORTED ON THE LATER HALF")
    print(" (read the out-of-sample column; in-sample is what it was chosen for)")
    print("")
    print("   %-46s %11s %11s" % ("policy", "in-sample", "OUT-OF-SAMPLE"))
    print("   %-46s %11s %11s" % ("", "mean ret", "mean ret"))
    for _, pol, tr, te in scored[:12]:
        print("   %-46s %+10.2f%% %+10.2f%%"
              % (pol["name"][:46], 100.0 * tr["mean"], 100.0 * te["mean"]))

    cur = [s for s in scored if s[1]["name"].startswith("current")]
    if cur:
        best_oos = max(scored, key=lambda x: x[3]["mean"])
        cur_oos = max(cur, key=lambda x: x[3]["mean"])
        print("")
        print(" current shipping policy, out-of-sample : %+.2f%% mean"
              % (100.0 * cur_oos[3]["mean"]))
        print(" best out-of-sample policy              : %+.2f%% mean  (%s)"
              % (100.0 * best_oos[3]["mean"], best_oos[1]["name"]))
        top_in = scored[0]
        print(" the policy that won IN-sample scores     %+.2f%% out-of-sample"
              % (100.0 * top_in[3]["mean"]))
        print(" -> if that last number is much worse than its in-sample score, the")
        print("    grid is fitting noise and no policy here should be shipped.")

    print("")
    print(" Mean return is per position, after a %d bps exit cost. It does NOT" % EXIT_COST_BPS)
    print(" include entry cost, which is already inside the recorded gain figures.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
