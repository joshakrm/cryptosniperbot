#!/usr/bin/env python3
"""Do the early buyers of winning tokens recur? The precondition for copy-trading.

Copy-trading is the one selection idea whose signal is not attacker-writable. A
scammer can put any string in a token metadata field for free - which is why
twitter links, telegram, website and description all measured null at n=1380 -
but a record of buying tokens that went up costs real money to build.

None of that matters if the winners have no repeat buyers. So this asks the
cheap question first, before any bot code is written: across tokens that
graduated, does the same wallet show up early more than once?

THE CONTROL IS THE WHOLE EXPERIMENT. A bot that snipes every launch appears on
every winner too, and looks like genius until you notice it also appears on
every loser. So each wallet is scored on HIT RATE - graduated bought over total
bought - against the base rate, not on how many winners it touched. A wallet
that bought 40 tokens of which 2 graduated is performing at the base rate and is
worth nothing.

AND IT REFUSES TO MANUFACTURE A NEGATIVE. The first version of this returned
None from the RPC layer on any exception, and the caller read that as "this
token had no early buyers". The endpoint answered 429 to essentially every
request, so 75 tokens produced 75 empty lists and it printed "no wallet appears
on more than one winner" - a clean, confident, entirely fabricated result. RPC
failures are counted now, and enough of them aborts the run instead of becoming
the conclusion.

publicnode 403s getSignaturesForAddress, so this needs an endpoint that permits
it, read from config.toml. This is an offline study, not part of the bot.

Usage:  python3 scripts/wallets.py [n_graduated] [n_control] [early_buyers]
"""
import collections
import io
import json
import os
import re
import sys
import threading
import time

try:
    from urllib.request import Request, urlopen
except ImportError:
    from urllib2 import Request, urlopen

CACHE = "wallet_cache.json"


def candidate_endpoints():
    """Anything that might serve signature history, configured one first.

    publicnode and drpc answer 403 to getSignaturesForAddress, and the Helius
    free tier answers 429 "max usage reached" once its credits are spent, so
    there is no single endpoint to hardcode. The configured one is tried first
    because it is the fastest when it works.
    """
    out = []
    for path in ("config.toml", "config-free.toml"):
        if not os.path.exists(path):
            continue
        txt = io.open(path, encoding="utf-8").read()
        m = re.search(r'http_url\s*=\s*"([^"]+)"', txt)
        if m and "publicnode" not in m.group(1) and m.group(1) not in out:
            out.append(m.group(1))
    out.append("https://api.mainnet-beta.solana.com")
    return out


def endpoint():
    """Probe for one that actually answers. A URL may carry a key, so only the
    host is ever printed."""
    probe = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
                        "params": ["So11111111111111111111111111111111111111112",
                                   {"limit": 1}]}).encode()
    for url in candidate_endpoints():
        host = url.split("/")[2] if "//" in url else url
        try:
            req = Request(url, probe, {"Content-Type": "application/json"})
            r = json.loads(urlopen(req, timeout=20).read().decode())
            if "error" not in r:
                print(" using endpoint: %s" % host)
                return url
            print(" skipping %s: %s" % (host, str(r["error"])[:60]))
        except Exception as e:
            print(" skipping %s: %s" % (host, str(e)[:60]))
        time.sleep(1.0)
    raise SystemExit(" No endpoint served getSignaturesForAddress. Cannot proceed.")


class RpcDead(Exception):
    """The endpoint stopped answering. NOT the same as a token having no buyers."""


class Rpc(object):
    def __init__(self, url, rate=1.0):
        self.url = url
        self.gap = 1.0 / rate
        self.last = [0.0]
        self.lock = threading.Lock()
        self.calls = 0
        self.failures = 0

    def call(self, method, params):
        with self.lock:
            wait = self.gap - (time.time() - self.last[0])
            if wait > 0:
                time.sleep(wait)
            self.last[0] = time.time()
            self.calls += 1
        body = json.dumps({"jsonrpc": "2.0", "id": 1,
                           "method": method, "params": params}).encode()
        backoff = 1.0
        for attempt in range(4):
            req = Request(self.url, body, {"Content-Type": "application/json"})
            try:
                r = json.loads(urlopen(req, timeout=25).read().decode())
                if "error" in r:
                    return None          # a real answer about this input
                return r.get("result")
            except Exception as e:
                msg = str(e)
                if "429" in msg or "503" in msg or "502" in msg:
                    # Throttling is a reason to slow down for the rest of the
                    # run, not just to sleep once - the whole run shares a budget.
                    with self.lock:
                        self.gap = min(self.gap * 1.5, 2.0)
                    time.sleep(backoff)
                    backoff *= 2
                    continue
                if attempt == 3:
                    break
                time.sleep(backoff)
                backoff *= 2
        with self.lock:
            self.failures += 1
        raise RpcDead(method + " failed after retries")

    def health(self):
        return self.calls, self.failures, 100.0 * self.failures / max(1, self.calls)


def oldest_signatures(rpc, address, want):
    """Walk back to the start of an account history and return the earliest ones."""
    before, page, pages = None, None, 0
    while pages < 6:
        params = [address, {"limit": 1000}]
        if before:
            params[1]["before"] = before
        got = rpc.call("getSignaturesForAddress", params)
        if not got:
            break
        page = got
        pages += 1
        if len(got) < 1000:
            break
        before = got[-1]["signature"]
    if not page:
        return []
    # A page is newest-first, so the earliest activity is its tail.
    return [s["signature"] for s in page[-want:]]


def buyers_of(rpc, curve, want, cache):
    """Early signers on a curve. Propagates RpcDead rather than returning [] -
    an empty list means nobody bought, and a rate limit must never assert that."""
    if curve in cache:
        return cache[curve]
    sigs = oldest_signatures(rpc, curve, want)
    out = []
    for sig in sigs:
        tx = rpc.call("getTransaction",
                      [sig, {"encoding": "jsonParsed",
                             "maxSupportedTransactionVersion": 0}])
        if not isinstance(tx, dict):
            continue
        try:
            keys = tx["transaction"]["message"]["accountKeys"]
        except (KeyError, TypeError):
            continue
        for k in keys:
            if k.get("signer"):
                out.append(k["pubkey"])
                break
    cache[curve] = out
    return out


def main():
    n_grad = int(sys.argv[1]) if len(sys.argv) > 1 else 30
    n_ctrl = int(sys.argv[2]) if len(sys.argv) > 2 else 60
    early = int(sys.argv[3]) if len(sys.argv) > 3 else 15

    meta = json.load(open("social_cache.json"))
    pump = [v for v in meta.values()
            if isinstance(v, dict) and v.get("bonding_curve") and not v.get("_error")]
    grad = [v for v in pump if v.get("complete")][:n_grad]
    ctrl = [v for v in pump if not v.get("complete")][:n_ctrl]

    if not grad:
        print("No graduated tokens in social_cache.json - run graduation.py first.")
        return 1

    cache = {}
    if os.path.exists(CACHE):
        try:
            cache = json.load(open(CACHE))
        except ValueError:
            cache = {}

    rpc = Rpc(endpoint())
    print("")
    print("=" * 78)
    print(" early-buyer recurrence: %d graduated, %d control, %d buyers each"
          % (len(grad), len(ctrl), early))
    print("=" * 78)

    bought = collections.defaultdict(lambda: [0, 0])   # wallet -> [graduated, total]
    resolved = {"graduated": 0, "control": 0}
    dead = 0
    for label, group, idx in (("graduated", grad, 0), ("control", ctrl, 1)):
        for i, t in enumerate(group):
            try:
                ws = buyers_of(rpc, t["bonding_curve"], early, cache)
            except RpcDead:
                dead += 1
                if dead >= 5:
                    json.dump(cache, open(CACHE, "w"))
                    calls, fails, rate = rpc.health()
                    print("")
                    print(" ABORTED: %d tokens unreadable (%d of %d calls failed, %.0f%%)."
                          % (dead, fails, calls, rate))
                    print(" That is an endpoint problem, not a result. Nothing is")
                    print(" concluded from a sample the RPC refused to serve.")
                    return 2
                continue
            if not ws:
                continue
            resolved[label] += 1
            for w in set(ws):
                bought[w][1] += 1
                if idx == 0:
                    bought[w][0] += 1
            if (i + 1) % 10 == 0:
                json.dump(cache, open(CACHE, "w"))
                print("  ... %s %d/%d" % (label, i + 1, len(group)))
    json.dump(cache, open(CACHE, "w"))

    calls, fails, rate = rpc.health()
    print("")
    print(" tokens actually read: %d graduated, %d control  (%d calls, %.0f%% failed)"
          % (resolved["graduated"], resolved["control"], calls, rate))
    if resolved["graduated"] < 5:
        print("")
        print(" Too few graduated tokens were readable to conclude anything.")
        return 2

    total_tokens = resolved["graduated"] + resolved["control"]
    base = 100.0 * resolved["graduated"] / max(1, total_tokens)
    print(" base rate in this sample: %.1f%% of tokens graduated" % base)
    print(" unique early-buyer wallets seen: %d" % len(bought))

    repeat = dict((w, v) for w, v in bought.items() if v[1] >= 2)
    print(" wallets seen on 2+ tokens: %d" % len(repeat))
    multi_win = dict((w, v) for w, v in bought.items() if v[0] >= 2)
    print(" wallets seen early on 2+ GRADUATED tokens: %d" % len(multi_win))

    if not multi_win:
        print("")
        print(" No wallet appears early on more than one winner in this sample.")
        print(" Copy-trading needs a repeat performer; widen the sample before")
        print(" concluding, but do not write bot code against this.")
        return 0

    print("")
    print(" CANDIDATE SMART MONEY - ranked by hit rate, not by wins")
    print(" (a wallet at or below the base rate is buying volume, not skill)")
    print("   %-46s %6s %6s %8s" % ("wallet", "grads", "total", "hit%"))
    ranked = sorted(multi_win.items(), key=lambda kv: -(kv[1][0] / float(kv[1][1])))
    for w, gt in ranked[:15]:
        g, t = gt
        hit = 100.0 * g / t
        flag = "  <-- beats base" if hit > base * 1.5 else ""
        print("   %-46s %6d %6d %7.1f%%%s" % (w, g, t, hit, flag))

    beats = [1 for w, gt in multi_win.items() if 100.0 * gt[0] / gt[1] > base * 1.5]
    print("")
    print(" %d wallet(s) beat 1.5x the base rate on 2+ winners." % len(beats))
    print("")
    print(" READ THIS BEFORE BELIEVING ANY ROW ABOVE. These wallets were chosen")
    print(" BECAUSE they did well in this sample, so of course they look good in")
    print(" it. That is selection, not prediction. The only thing that would mean")
    print(" anything is picking wallets on one time period and measuring them on a")
    print(" later one they had no part in choosing.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
