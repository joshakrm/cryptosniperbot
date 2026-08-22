#!/usr/bin/env python3
"""Does a token's declared social presence predict how the trade went?

The bot has no ALPHA filter - nothing that selects for tokens likely to rise.
Social presence is the obvious candidate, and pump.fun publishes it: their
public coin endpoint carries the twitter/telegram/website a creator declared,
and it is populated within about two seconds of launch.

This joins that against the trades actually taken and asks whether it separates
winners from losers.

Read the n before believing anything here. The sample is trades that PASSED
screening, so it says nothing about the candidates that were rejected, and with
single-digit wins no split can be significant. It is here to size an effect and
to measure fill rates, not to settle the question.

Usage:  python3 scripts/social.py
"""
import json, glob, os, time, collections
try:
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError, URLError
except ImportError:
    from urllib2 import Request, urlopen, HTTPError, URLError

API = "https://frontend-api-v3.pump.fun/coins/"
CACHE = "social_cache.json"


def closed_trades():
    """Deduplicated closed trades. Journals overlap; counting them twice has
    produced a wrong P&L in this project before."""
    seen = {}
    for p in glob.glob("journal*.jsonl"):
        for line in open(p, encoding="utf-8", errors="replace"):
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("kind") == "position_close":
                d = r["data"]
                seen[(d.get("mint"), r.get("ts"))] = d
    return list(seen.values())


def fetch(mint, cache):
    if mint in cache:
        return cache[mint]
    req = Request(API + mint, headers={"User-Agent": "Mozilla/5.0"})
    try:
        body = urlopen(req, timeout=12).read().decode("utf-8", "replace")
        cache[mint] = json.loads(body)
    except HTTPError as e:
        cache[mint] = {"_error": "http %d" % e.code}
    except (URLError, ValueError, OSError) as e:
        cache[mint] = {"_error": str(e)[:60]}
    time.sleep(0.4)
    return cache[mint]


def summarise(label, group):
    if not group:
        print("   %-30s n=0" % label)
        return
    pnls = [g["pnl_sol"] for g in group]
    wins = [p for p in pnls if p > 0]
    peaks = [g.get("max_gain_bps") for g in group if g.get("max_gain_bps") is not None]
    line = "   %-30s n=%-4d  win %4.1f%%  mean %+.4f  net %+.3f" % (
        label, len(group), 100.0 * len(wins) / len(pnls), sum(pnls) / len(pnls), sum(pnls))
    if peaks:
        peaks.sort()
        line += "  med peak %+5d bps" % peaks[len(peaks) // 2]
    print(line)


def main():
    cache = {}
    if os.path.exists(CACHE):
        try:
            cache = json.load(open(CACHE))
        except ValueError:
            cache = {}

    trades = closed_trades()
    print("")
    print("=" * 78)
    print(" social signal vs outcome: %d deduplicated closed trades" % len(trades))
    print("=" * 78)

    rows = []
    for i, t in enumerate(trades):
        meta = fetch(t["mint"], cache)
        if i % 20 == 19:
            json.dump(cache, open(CACHE, "w"))
        if meta.get("_error"):
            rows.append((t, None))
        else:
            rows.append((t, meta))
    json.dump(cache, open(CACHE, "w"))

    resolved = [(t, m) for t, m in rows if m]
    missing = len(rows) - len(resolved)
    print("")
    print(" metadata resolved for %d of %d (%d not indexed by pump.fun)"
          % (len(resolved), len(rows), missing))

    def has(m, field):
        v = (m.get(field) or "").strip()
        return bool(v)

    print("")
    print(" FILL RATES (does the field even discriminate?)")
    for field in ("twitter", "telegram", "website", "description"):
        n = sum(1 for _, m in resolved if has(m, field))
        print("   %-12s present on %3d / %3d  (%4.1f%%)"
              % (field, n, len(resolved), 100.0 * n / max(1, len(resolved))))

    print("")
    print(" OUTCOME BY DECLARED SOCIALS")
    for field in ("twitter", "telegram", "website"):
        summarise("has %s" % field, [t for t, m in resolved if has(m, field)])
        summarise("no %s" % field, [t for t, m in resolved if not has(m, field)])
        print("")

    print(" TWITTER LINK TYPE (a tweet is not a profile)")
    tweet = [t for t, m in resolved if "/status/" in (m.get("twitter") or "")]
    prof = [t for t, m in resolved
            if has(m, "twitter") and "/status/" not in (m.get("twitter") or "")]
    summarise("links a specific tweet", tweet)
    summarise("links a profile", prof)

    print("")
    print(" SERIAL DEPLOYERS (same creator seen more than once)")
    creators = collections.Counter(m.get("creator") for _, m in resolved if m.get("creator"))
    repeat = set(c for c, n in creators.items() if n > 1)
    summarise("creator seen >1x", [t for t, m in resolved if m.get("creator") in repeat])
    summarise("creator seen once", [t for t, m in resolved if m.get("creator") not in repeat])

    print("")
    print(" REPLY COUNT AT READ TIME")
    for lo, hi, lbl in ((0, 1, "0 replies"), (1, 5, "1-4"), (5, 10**9, "5+")):
        summarise(lbl, [t for t, m in resolved
                        if lo <= (m.get("reply_count") or 0) < hi])

    wins = sum(1 for t, _ in resolved if t["pnl_sol"] > 0)
    print("")
    print(" NOTE: %d wins in the whole sample. Any split above rests on fewer than" % wins)
    print(" that, which cannot separate a real effect from noise. Treat every line")
    print(" as an effect SIZE to test, not a result.")
    print("")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
