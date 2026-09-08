#!/usr/bin/env python3
"""Find wallets worth copying, and refuse to be fooled by the ones that were lucky.

Copy-trading needs a list of wallets. This builds one from Solana history alone -
no third-party service, no leaderboard, nothing whose terms govern what we may
read. The outcome label comes from the bonding curve account itself, which
carries a `complete` flag once the curve has filled: an on-chain fact, one
getAccountInfo per token.

THE METHOD, AND WHY EACH PART IS THERE.

A wallet that buys 400 tokens will appear early in plenty of winners purely by
volume, and will look like genius to anyone counting winners. So wallets are
scored on HIT RATE against the base rate, never on how many winners they touched.

Selecting the wallets that did well and then reporting how well they did is
circular - it measures the selection, not the wallet. So tokens are split BY TIME:
wallets are ranked on the earlier half and scored on the later half, which they
had no part in choosing. Only the second number means anything, and a previous
version of this analysis in this project produced 8 apparent repeat-winners
against 11.1 expected by chance, which is what the circular version hides.

Deployers are excluded. The first signer on a curve created the token; following
them is not copy-trading, and the one wallet that looked spectacular last time -
7 winners from 7 - turned out to be a deployer whose tokens all collapsed to a
$2 market cap after filling their curves.

AND IT REFUSES TO TURN FAILURES INTO FINDINGS. An RPC that answers 429 is not a
token with no buyers. Batches silently return fewer results than requested -
measured here: 25 requested, 3 returned - so every fetch is reconciled against
what was asked for, and a token that could not be read is dropped from BOTH the
numerator and the denominator rather than counted as a miss.

Usage:
    python3 scripts/rank_wallets.py [n_tokens] [early_n]
"""
import collections
import io
import json
import os
import random
import struct
import sys
import time

try:
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError
except ImportError:                                    # pragma: no cover
    from urllib2 import Request, urlopen, HTTPError

OFFICIAL = "https://api.mainnet-beta.solana.com"
CURVE_CACHE = "curve_outcomes.json"
BUYER_CACHE = "early_buyers.json"

# Measured on this endpoint: a batch of 10 returns ~4 tx/sec, 25 returns partial,
# 50 answers 429. Small batches with a pause beat large ones that fail.
BATCH = 10
PAUSE = 1.2


class Rpc(object):
    """JSON-RPC that counts what it failed to get, so silence is never a result."""

    def __init__(self, url=OFFICIAL):
        self.url = url
        self.calls = 0
        self.failures = 0

    def _post(self, payload, timeout=60):
        body = json.dumps(payload).encode()
        req = Request(self.url, body, {"Content-Type": "application/json"})
        return json.loads(urlopen(req, timeout=timeout).read().decode())

    def one(self, method, params):
        self.calls += 1
        backoff = 1.5
        for attempt in range(4):
            try:
                r = self._post({"jsonrpc": "2.0", "id": 1,
                                "method": method, "params": params})
                if "error" in r:
                    return None            # a real answer about this input
                return r.get("result")
            except HTTPError as e:
                if e.code in (429, 502, 503):
                    time.sleep(backoff)
                    backoff *= 2
                    continue
                break
            except Exception:
                time.sleep(backoff)
                backoff *= 2
        self.failures += 1
        return None

    def batch_tx(self, sigs):
        """Fetch transactions, returning {sig: tx} for those that ACTUALLY came
        back. Missing entries are reported by absence, never as empty results."""
        out = {}
        for i in range(0, len(sigs), BATCH):
            chunk = sigs[i:i + BATCH]
            self.calls += len(chunk)
            payload = [{"jsonrpc": "2.0", "id": j, "method": "getTransaction",
                        "params": [s, {"encoding": "jsonParsed",
                                       "maxSupportedTransactionVersion": 0}]}
                       for j, s in enumerate(chunk)]
            got = None
            backoff = 1.5
            for attempt in range(4):
                try:
                    got = self._post(payload)
                    break
                except HTTPError as e:
                    if e.code in (429, 502, 503):
                        time.sleep(backoff)
                        backoff *= 2
                        continue
                    break
                except Exception:
                    time.sleep(backoff)
                    backoff *= 2
            if not isinstance(got, list):
                self.failures += len(chunk)
                time.sleep(PAUSE)
                continue
            for item in got:
                idx = item.get("id")
                if idx is None or not isinstance(idx, int) or idx >= len(chunk):
                    continue
                if item.get("result"):
                    out[chunk[idx]] = item["result"]
            self.failures += len(chunk) - sum(1 for s in chunk if s in out)
            time.sleep(PAUSE)
        return out


def journal_mints():
    """pump.fun mints this bot has seen, newest first, with the time seen."""
    import glob
    seen = {}
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
            if d.get("venue") != "pump_fun":
                continue
            seen.setdefault(d["mint"], (r.get("ts", ""), d.get("signature")))
    return seen


def curve_outcome(rpc, mint, sig, cache):
    """(complete, curve_address) for a mint, read from the chain.

    `complete` means the bonding curve filled - roughly 85 SOL of net buying and
    a large move up. It is the honest on-chain outcome label and needs no
    third-party service.
    """
    if mint in cache:
        return cache[mint]
    tx = rpc.one("getTransaction",
                 [sig, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}])
    if not tx:
        return None                      # could not read: not an outcome
    meta = tx.get("meta") or {}
    keys = tx.get("transaction", {}).get("message", {}).get("accountKeys", [])
    payer = next((k.get("pubkey") for k in keys if k.get("signer")), None)
    curve, best = None, 0
    for b in (meta.get("postTokenBalances") or []):
        if b.get("mint") != mint:
            continue
        owner = b.get("owner")
        if not owner or owner == payer:
            continue
        try:
            amt = int((b.get("uiTokenAmount") or {}).get("amount") or 0)
        except (TypeError, ValueError):
            amt = 0
        if amt > best:
            best, curve = amt, owner
    if not curve:
        return None
    acct = rpc.one("getAccountInfo", [curve, {"encoding": "base64"}])
    val = (acct or {}).get("value")
    if not val:
        # A closed account means the curve completed and migrated.
        cache[mint] = [True, curve]
        return cache[mint]
    import base64
    raw = base64.b64decode(val["data"][0])
    if len(raw) < 49:
        return None
    complete = raw[48] != 0
    cache[mint] = [bool(complete), curve]
    return cache[mint]


def early_buyers(rpc, mint, curve, n, cache):
    """The first `n` distinct signers to trade this curve, in order.

    Index 0 is the creation transaction, so the deployer is dropped: following
    whoever made the token is not copy-trading, and the standout wallet in an
    earlier version of this analysis was exactly that.
    """
    key = "%s:%d" % (curve, n)
    if key in cache:
        return cache[key]
    # walk back to the beginning of the curve's history
    before, page, pages = None, None, 0
    while pages < 6:
        params = [curve, {"limit": 1000}]
        if before:
            params[1]["before"] = before
        got = rpc.one("getSignaturesForAddress", params)
        if got is None:
            return None                  # RPC failure, not an empty history
        if not got:
            break
        page = got
        pages += 1
        if len(got) < 1000:
            break
        before = got[-1]["signature"]
    if not page:
        return None
    sigs = [s["signature"] for s in page[-(n + 1):]]
    txs = rpc.batch_tx(sigs)
    if len(txs) < max(2, len(sigs) // 2):
        return None                      # too much of it missing to trust
    ordered = []
    for s in sigs:
        tx = txs.get(s)
        if not tx:
            continue
        keys = tx.get("transaction", {}).get("message", {}).get("accountKeys", [])
        signer = next((k.get("pubkey") for k in keys if k.get("signer")), None)
        if signer and signer not in ordered:
            ordered.append(signer)
    cache[key] = ordered[1:]             # drop the deployer
    return cache[key]


def main():
    n_tokens = int(sys.argv[1]) if len(sys.argv) > 1 else 240
    early_n = int(sys.argv[2]) if len(sys.argv) > 2 else 20

    curves = json.load(io.open(CURVE_CACHE)) if os.path.exists(CURVE_CACHE) else {}
    buyers = json.load(io.open(BUYER_CACHE)) if os.path.exists(BUYER_CACHE) else {}

    seen = journal_mints()
    ordered = sorted(seen.items(), key=lambda kv: kv[1][0])   # oldest first
    ordered = [(m, ts, sig) for m, (ts, sig) in ordered if sig]
    if len(ordered) > n_tokens:
        step = len(ordered) / float(n_tokens)
        ordered = [ordered[int(i * step)] for i in range(n_tokens)]

    rpc = Rpc()
    print("")
    print("=" * 78)
    print(" wallet ranking: %d tokens, %d early buyers each" % (len(ordered), early_n))
    print("=" * 78)
    print(" outcome label: the bonding curve's own `complete` flag, read on chain")

    rows = []
    for i, (mint, ts, sig) in enumerate(ordered):
        out = curve_outcome(rpc, mint, sig, curves)
        if not out:
            continue
        complete, curve = out
        eb = early_buyers(rpc, mint, curve, early_n, buyers)
        if eb is None:
            continue
        rows.append((ts, mint, bool(complete), eb))
        if (i + 1) % 20 == 0:
            json.dump(curves, io.open(CURVE_CACHE, "w"))
            json.dump(buyers, io.open(BUYER_CACHE, "w"))
            print("   ... %d/%d processed, %d usable, %d rpc failures"
                  % (i + 1, len(ordered), len(rows), rpc.failures))
    json.dump(curves, io.open(CURVE_CACHE, "w"))
    json.dump(buyers, io.open(BUYER_CACHE, "w"))

    print("")
    print(" tokens read: %d of %d attempted   (%d rpc calls, %d failed)"
          % (len(rows), len(ordered), rpc.calls, rpc.failures))
    if len(rows) < 40:
        print("")
        print(" Too few tokens were readable to conclude anything. This is an")
        print(" endpoint limit, not a result about wallets.")
        return 2

    wins = sum(1 for _, _, c, _ in rows if c)
    print(" of those, %d filled their curve (%.1f%% base rate)"
          % (wins, 100.0 * wins / len(rows)))
    if wins < 10:
        print("")
        print(" Fewer than 10 winners in the sample. Nothing can be ranked against")
        print(" a base rate this thin - widen n_tokens.")
        return 2

    rows.sort()
    half = len(rows) // 2
    train, test = rows[:half], rows[half:]

    def tally(sub):
        t = collections.defaultdict(lambda: [0, 0])       # wallet -> [wins, total]
        for _, _, complete, eb in sub:
            for w in set(eb):
                t[w][1] += 1
                if complete:
                    t[w][0] += 1
        return t

    tr, te = tally(train), tally(test)
    tr_base = 100.0 * sum(1 for _, _, c, _ in train if c) / max(1, len(train))
    te_base = 100.0 * sum(1 for _, _, c, _ in test if c) / max(1, len(test))
    print(" split by time: %d earlier (rank on) / %d later (score on)"
          % (len(train), len(test)))
    print(" base rate  earlier %.1f%%   later %.1f%%" % (tr_base, te_base))

    # Rank on the earlier half. Require enough appearances that a hit rate means
    # something: two-from-two is a coin landing heads twice.
    MIN_SEEN = 4
    ranked = [(w, v[0], v[1], 100.0 * v[0] / v[1])
              for w, v in tr.items() if v[1] >= MIN_SEEN]
    ranked.sort(key=lambda r: (-r[3], -r[2]))
    print("")
    print(" TOP WALLETS ON THE EARLIER HALF (seen on %d+ tokens)" % MIN_SEEN)
    if not ranked:
        print("   none - no wallet appeared early on %d tokens in this sample." % MIN_SEEN)
        return 0
    print("   %-46s %6s %6s %8s %10s" % ("wallet", "wins", "seen", "hit%", "later hit%"))
    followed = []
    for w, wn, sn, hit in ranked[:15]:
        later = te.get(w)
        lstr = "%.0f%% (%d)" % (100.0 * later[0] / later[1], later[1]) if later and later[1] else "not seen"
        print("   %-46s %6d %6d %7.1f%% %10s" % (w, wn, sn, hit, lstr))
        followed.append(w)

    # The only number that matters: do the wallets chosen on the earlier half
    # beat the base rate on the LATER half, which they had no part in choosing?
    top = set(w for w, _, _, hit in ranked[:15] if hit > tr_base)
    tot = sum(te[w][1] for w in top if w in te)
    won = sum(te[w][0] for w in top if w in te)
    print("")
    print(" OUT-OF-SAMPLE - the only result worth reading")
    if tot == 0:
        print("   The wallets picked on the earlier half never appeared in the later")
        print("   half at all. Nothing to follow: they were not persistent traders,")
        print("   they were present for a while and then gone.")
        return 0
    hit = 100.0 * won / tot
    print("   picked %d wallets on the earlier half" % len(top))
    print("   on the later half they were early on %d tokens, %d of which filled" % (tot, won))
    print("   their hit rate %.1f%%  vs base rate %.1f%%" % (hit, te_base))

    # Is that difference distinguishable from chance? Shuffle which tokens won,
    # keeping how often each wallet appeared, and see how often chance does better.
    random.seed(11)
    labels = [c for _, _, c, _ in test]
    appear = [te[w][1] for w in top if w in te]
    better = 0
    SIMS = 3000
    for _ in range(SIMS):
        lab = labels[:]
        random.shuffle(lab)
        idx = list(range(len(test)))
        w_sim = 0
        for a in appear:
            pick = random.sample(idx, min(a, len(idx)))
            w_sim += sum(1 for i in pick if lab[i])
        if w_sim >= won:
            better += 1
    p = better / float(SIMS)
    print("   p = %.3f  (%d of %d shuffles did as well by chance)" % (p, better, SIMS))
    print("")
    if p < 0.05 and hit > te_base:
        print(" A real effect at this sample size. These wallets are worth")
        print(" following - put them in config-copy.toml and let the bot mirror")
        print(" them on paper before believing anything further.")
    else:
        print(" NOT distinguishable from chance. Picking the best wallets on one")
        print(" period does not predict the next, which is what luck looks like.")
        print(" Do not put these in a config. Widen the sample and re-run, or")
        print(" accept that this population has no persistent skill to copy.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
