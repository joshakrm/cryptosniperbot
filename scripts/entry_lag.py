#!/usr/bin/env python3
"""What does arriving late cost a copy-trader?

Sniping died on latency: detection-to-fill is 5.7s median against a ~75ms
competitive benchmark, and the median sniped position peaked BELOW its own fill.
Copy-trading only makes sense if that same lag is survivable, and whether it is
depends entirely on how fast the price moves in the seconds after a followed
wallet buys.

So this measures exactly that. For each token it takes the earliest transactions,
computes the bonding curve price at each from the curve's own reserves, and
reports what the price had done 6, 15, 30 and 60 seconds after the first buy.

BOTH ARMS, DELIBERATELY. An earlier pass measured two tokens, both of which
graduated - tokens selected on their outcome, where the price rising afterwards
is close to guaranteed. A follower cannot know the outcome at entry, so the cost
of lag has to be measured across what they would actually be buying: the
winners and the losers together.

PRICE FROM RESERVES, NOT FROM THE TRADER'S DELTAS. Inferring price from a
signer's SOL and token movement gave figures twelve orders of magnitude apart
within the same second - multi-hop routes, rent, and fees all land in the same
delta. The curve holds the reserves that define the price, so the price is read
from there instead, and the curve address is LOCKED from the first transaction:
letting it be re-identified per transaction picked up the pool at migration and
produced prices of 9.77 SOL per token.

Usage:  python3 scripts/entry_lag.py [n_per_arm]
"""
import io
import json
import sys
import time

try:
    from urllib.request import Request, urlopen
except ImportError:                                   # pragma: no cover
    from urllib2 import Request, urlopen

OFFICIAL = "https://api.mainnet-beta.solana.com"
# pump.fun's curve starts at 30 virtual SOL against 1.073e9 virtual tokens.
VIRTUAL_SOL_0 = 30.0
# A fresh curve prices a token near 2.8e-8 SOL. Anything far outside this band is
# a misread rather than a market move, and is dropped rather than averaged in.
PRICE_MIN, PRICE_MAX = 1.0e-9, 1.0e-5


def rpc(method, params, tries=5):
    backoff = 2.0
    for _ in range(tries):
        try:
            body = json.dumps({"jsonrpc": "2.0", "id": 1,
                               "method": method, "params": params}).encode()
            req = Request(OFFICIAL, body, {"Content-Type": "application/json"})
            r = json.loads(urlopen(req, timeout=40).read().decode())
            if "error" in r:
                return None
            return r.get("result")
        except Exception:
            time.sleep(backoff)
            backoff *= 1.6
    return None


def find_curve(tx, mint):
    """The account holding the token supply - the bonding curve."""
    meta = tx.get("meta") or {}
    keys = tx.get("transaction", {}).get("message", {}).get("accountKeys", [])
    payer = next((k.get("pubkey") for k in keys if k.get("signer")), None)
    best, owner = 0.0, None
    for b in (meta.get("postTokenBalances") or []):
        if b.get("mint") != mint:
            continue
        o = b.get("owner")
        if not o or o == payer:
            continue
        try:
            a = float((b.get("uiTokenAmount") or {}).get("uiAmountString") or 0)
        except (TypeError, ValueError):
            continue
        if a > best:
            best, owner = a, o
    return owner


def price_from(tx, mint, curve):
    """Curve price after this transaction, using the LOCKED curve address."""
    meta = tx.get("meta") or {}
    keys = [k.get("pubkey") for k in
            tx.get("transaction", {}).get("message", {}).get("accountKeys", [])]
    tokens = None
    for b in (meta.get("postTokenBalances") or []):
        if b.get("mint") == mint and b.get("owner") == curve:
            try:
                tokens = float((b.get("uiTokenAmount") or {}).get("uiAmountString") or 0)
            except (TypeError, ValueError):
                return None
    if not tokens or tokens <= 0:
        return None
    if curve not in keys:
        return None
    lamports = (meta.get("postBalances") or [0] * len(keys))[keys.index(curve)] / 1e9
    price = (VIRTUAL_SOL_0 + lamports) / tokens
    if not (PRICE_MIN <= price <= PRICE_MAX):
        return None                     # a misread, not a market move
    return price


def token_path(mint, depth=26):
    """[(seconds_since_first_buy, price)] over the token's earliest activity."""
    sigs = rpc("getSignaturesForAddress", [mint, {"limit": 1000}])
    if not sigs:
        return None
    oldest = sorted(sigs, key=lambda s: s.get("blockTime") or 0)[:depth]
    curve, t0, out = None, None, []
    for s in oldest:
        tx = rpc("getTransaction", [s["signature"],
                                    {"encoding": "jsonParsed",
                                     "maxSupportedTransactionVersion": 0}])
        time.sleep(0.85)
        if not tx:
            continue
        if curve is None:
            curve = find_curve(tx, mint)
            if curve is None:
                continue
        p = price_from(tx, mint, curve)
        if p is None:
            continue
        bt = s.get("blockTime") or 0
        if t0 is None:
            t0 = bt
        out.append((bt - t0, p))
    return out if len(out) >= 4 else None


def at_lag(path, lag):
    """Price at the first observation `lag` seconds or more after the first."""
    if not path:
        return None
    base = path[0][1]
    for dt, p in path:
        if dt >= lag:
            return 100.0 * (p / base - 1.0)
    return None


def main():
    per_arm = int(sys.argv[1]) if len(sys.argv) > 1 else 15
    cache = {}
    try:
        cache = json.load(io.open("early_buyers.json"))
    except Exception:
        pass

    import glob
    winners, losers = {}, {}
    for p in glob.glob("journal*.jsonl"):
        for line in io.open(p, encoding="utf-8", errors="replace"):
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("kind") != "candidate":
                continue
            d = r["data"]
            m, v, ts = d.get("mint"), d.get("venue"), r.get("ts", "")
            if not m:
                continue
            if v == "pump_swap" and m.endswith("pump") and (d.get("pool_sol") or 0) > 50:
                winners.setdefault(m, ts)
            elif v == "pump_fun":
                losers.setdefault(m, ts)
    for m in winners:
        losers.pop(m, None)

    def spread(d, n):
        items = sorted(d.items(), key=lambda kv: kv[1])
        if len(items) > n:
            step = len(items) / float(n)
            items = [items[int(i * step)] for i in range(n)]
        return [m for m, _ in items]

    arms = [("graduated", spread(winners, per_arm)),
            ("did not graduate", spread(losers, per_arm))]

    print("")
    print("=" * 78)
    print(" cost of arriving late: curve price after the first buy")
    print("=" * 78)
    print(" both arms measured, because a follower cannot know the outcome at entry")

    results = {}
    for label, mints in arms:
        rows = []
        for m in mints:
            path = token_path(m)
            if path:
                rows.append(path)
        results[label] = rows
        print("")
        print(" %s: %d of %d tokens priced" % (label, len(rows), len(mints)))
        if not rows:
            continue
        print("   %-10s %10s %10s %10s" % ("lag", "median", "25th", "75th"))
        for lag in (6, 15, 30, 60):
            vals = [v for v in (at_lag(p, lag) for p in rows) if v is not None]
            if not vals:
                print("   %-10s %10s" % ("+%ds" % lag, "no data"))
                continue
            vals.sort()
            q = lambda f: vals[min(len(vals) - 1, int(f * len(vals)))]
            print("   %-10s %+9.1f%% %+9.1f%% %+9.1f%%  (n=%d)"
                  % ("+%ds" % lag, vals[len(vals) // 2], q(0.25), q(0.75), len(vals)))

    print("")
    print(" WHAT THIS DECIDES")
    print(" Our detection-to-fill is 5.7s median, 14.3s p90. If the +6s and +15s")
    print(" rows are small, the lag that killed sniping does not kill copying -")
    print(" the edge would be in choosing a token, not in beating anyone to it.")
    print(" If they are large, a follower buys the move rather than the signal.")
    both = [v for label in results for p in results[label]
            for v in [at_lag(p, 6)] if v is not None]
    if both:
        both.sort()
        print("")
        print(" ACROSS BOTH ARMS, the cost of a 6 second lag is a median of %+.1f%%"
              % both[len(both) // 2])
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
