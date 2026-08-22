#!/usr/bin/env python3
"""Which at-launch signals predict a token graduating?

The shadow tracker exists because every P&L figure here describes tokens that
PASSED screening, which cannot tell a filter that removes losers from one that
removes winners. It answers that by following a sample of untraded candidates -
slowly, a few per hour.

pump.fun answers it for free and retroactively. Their public coin endpoint
reports `complete`, meaning the token filled its bonding curve and graduated to
a real pool. Graduation is a coarse outcome but an unambiguous one: it takes
roughly 85 SOL of net buying, so a graduated token went up a great deal and a
non-graduated one did not. Every candidate ever journalled can be labelled this
way, including the thousands that were rejected and never traded.

So this asks the selection question directly: among candidates seen at launch,
which freely-observable property separates the ones that went on to graduate?

THE CIRCULARITY THAT MUST BE CONTROLLED FOR. A pump_swap candidate IS a
migration, and a migration is a token that has already graduated - so
`complete` is true there by construction, not by outcome. Measured: pump_swap
graduates at 96.97% against pump_fun's 2.17%. Leaving those 33 rows in made
pool_sol look like a perfect predictor (19 of 19 above 20 SOL "graduated")
when the 20+ SOL band simply IS the migration band, at a fixed 67.41 or 85.01
SOL. Every headline in the uncontrolled run came from this. So this script
now analyses one venue at a time and never pools them.

CAVEATS, because they matter more than the numbers:
  - Graduation is measured NOW, so a candidate seen an hour ago has had less
    time to graduate than one seen three days ago. Compare within an age band
    before believing a difference.
  - Graduating is not the same as being profitable to snipe. The bot holds for
    at most 15 minutes; graduation can take hours. A signal that predicts
    graduation is a lead, not a strategy.
  - This is observational. A property correlated with graduation may be a
    consequence of the buying rather than a cause of it.

Usage:  python3 scripts/graduation.py [limit]
"""
import json, glob, os, sys, time, collections, threading
try:
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError, URLError
except ImportError:
    from urllib2 import Request, urlopen, HTTPError, URLError

API = "https://frontend-api-v3.pump.fun/coins/"
CACHE = "social_cache.json"
WORKERS = 3
SLEEP = 0.6          # per worker, so about 5 requests/second in total


def candidates():
    out = {}
    for p in glob.glob("journal*.jsonl"):
        for line in open(p, encoding="utf-8", errors="replace"):
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("kind") == "candidate":
                out.setdefault(r["data"]["mint"], r["data"])
    return out


def fetch_all(mints, cache, lock):
    todo = [m for m in mints if m not in cache]
    print("fetching %d (cached %d)" % (len(todo), len(mints) - len(todo)))
    idx = [0]

    def worker():
        while True:
            with lock:
                i = idx[0]
                idx[0] += 1
            if i >= len(todo):
                return
            m = todo[i]
            req = Request(API + m, headers={"User-Agent": "Mozilla/5.0"})
            try:
                body = urlopen(req, timeout=12).read().decode("utf-8", "replace")
                val = json.loads(body)
            except HTTPError as e:
                val = {"_error": "http %d" % e.code}
            except (URLError, ValueError, OSError) as e:
                val = {"_error": str(e)[:60]}
            with lock:
                cache[m] = val
                if len(cache) % 100 == 0:
                    json.dump(cache, open(CACHE, "w"))
                    print("  ... %d done" % len(cache))
            time.sleep(SLEEP)

    ts = [threading.Thread(target=worker) for _ in range(WORKERS)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    json.dump(cache, open(CACHE, "w"))


def rate(label, group, base=None):
    if not group:
        print("   %-28s n=0" % label)
        return None
    g = sum(1 for x in group if x.get("complete"))
    pct = 100.0 * g / len(group)
    caps = sorted(x.get("usd_market_cap") or 0.0 for x in group)
    line = "   %-28s n=%-5d graduated %3d (%5.2f%%)  median mcap $%,.0f".replace(",", "") % (
        label, len(group), g, pct, caps[len(caps) // 2])
    if base is not None and base > 0:
        line += "   %+.2fx base" % (pct / base)
    print(line)
    return pct


def main():
    limit = int(sys.argv[1]) if len(sys.argv) > 1 else 1500
    cache = {}
    if os.path.exists(CACHE):
        try:
            cache = json.load(open(CACHE))
        except ValueError:
            cache = {}

    cands = candidates()
    mints = sorted(cands)[:limit]
    lock = threading.Lock()
    fetch_all(mints, cache, lock)

    rows = []
    for m in mints:
        meta = cache.get(m) or {}
        if meta.get("_error"):
            continue
        meta["_j"] = cands[m]
        rows.append(meta)

    print("")
    print("=" * 78)
    print(" graduation analysis: %d candidates resolved of %d" % (len(rows), len(mints)))
    print("=" * 78)

    # Venue first, and loudly, because pooling them is the one error that makes
    # everything downstream look spectacular and mean nothing.
    byv = collections.defaultdict(list)
    for r in rows:
        byv[r["_j"].get("venue") or "?"].append(r)
    print("")
    print(" GRADUATION BY VENUE - read this before anything else")
    for v, g in sorted(byv.items(), key=lambda kv: -len(kv[1])):
        gr = sum(1 for x in g if x.get("complete"))
        print("   %-14s n=%-5d graduated %3d (%6.2f%%)" % (v, len(g), gr, 100.0 * gr / len(g)))
    print("   A migration venue graduates at ~100% by construction: the token had")
    print("   already graduated before the bot ever saw it. Those rows are excluded")
    print("   below rather than pooled.")

    rows = byv.get("pump_fun") or []
    if not rows:
        print("")
        print(" No pump_fun launches in this sample - nothing to analyse.")
        return 1
    print("")
    print("=" * 78)
    print(" pump_fun launches only: n=%d" % len(rows))
    print("=" * 78)
    base = rate("ALL (base rate)", rows)
    print("")

    def tw(m):
        v = (m.get("twitter") or "").strip()
        if not v:
            return "none"
        return "tweet" if "/status/" in v else "profile"

    print(" BY TWITTER LINK TYPE")
    for k in ("profile", "tweet", "none"):
        rate(k, [r for r in rows if tw(r) == k], base)

    print("")
    print(" BY DECLARED FIELDS")
    for f in ("website", "telegram", "description"):
        rate("has " + f, [r for r in rows if (r.get(f) or "").strip()], base)
        rate("no " + f, [r for r in rows if not (r.get(f) or "").strip()], base)

    print("")
    print(" BY POOL SOL AT LAUNCH (what the free gate reads)")
    print(" NOTE: within pump_fun this was flat - 1.96%/0.00%/1.75%/2.56% across")
    print(" the bands. The apparent pool_sol effect was entirely the migration venue.")
    for lo, hi, lbl in ((0, 1, "< 1 SOL"), (1, 2, "1-2"), (2, 5, "2-5"),
                        (5, 20, "5-20"), (20, 1e9, "20+")):
        rate(lbl, [r for r in rows
                   if lo <= (r["_j"].get("pool_sol") or 0) < hi], base)

    print("")
    print(" BY CREATOR SHARE AT LAUNCH")
    for lo, hi, lbl in ((0, 2, "< 2%"), (2, 5, "2-5%"), (5, 10, "5-10%"),
                        (10, 1e9, "10%+")):
        rate(lbl, [r for r in rows
                   if lo <= (r["_j"].get("creator_share_pct") or 0) < hi], base)

    print("")
    print(" BY SERIAL DEPLOYER")
    cc = collections.Counter(r.get("creator") for r in rows if r.get("creator"))
    rep = set(c for c, n in cc.items() if n > 1)
    rate("creator seen >1x", [r for r in rows if r.get("creator") in rep], base)
    rate("creator seen once", [r for r in rows if r.get("creator") not in rep], base)

    grads = sum(1 for r in rows if r.get("complete"))
    print("")
    print(" %d graduations in %d candidates. A split needs roughly 30 graduations"
          % (grads, len(rows)))
    print(" per side before a difference means anything; check the n before acting.")
    print(" Graduation also takes hours, and the bot holds for 15 minutes - a signal")
    print(" here is a lead to test, not a strategy to ship.")
    print("")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
