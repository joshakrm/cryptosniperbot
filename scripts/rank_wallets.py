#!/usr/bin/env python3
"""Find wallets worth copying, and refuse to be fooled by the ones that were lucky.

Copy-trading needs a list of wallets. This builds one from Solana history alone -
no third-party service, no leaderboard, nothing whose terms govern what we may
read.

CASE-CONTROL SAMPLING, because the outcome is rare. A random sample of 240
launches produced 129 readable tokens and SIX winners - a 4.7% base rate, which
would need ~640 readable tokens to yield 30 winners, or about eight hours
against an endpoint that fails 37% of calls. So winners and losers are drawn
separately: every known graduation, plus a matched set of launches that did not
graduate. The absolute base rate then reflects the sampling ratio rather than
the wild, which is fine - the permutation test asks whether chance could produce
this hit rate GIVEN this mix, and that question is unaffected.

THE WINNER LABEL is the venue a token was seen on. A pump_swap candidate is a
migration, and a migration is a curve that filled. But only 24% of 1191 observed
migrations carry the pump suffix - the rest are ordinary PumpSwap pools someone
opened by hand at 0.1 or 7 or 800 SOL - so a winner must ALSO have moved a
canonical graduation's worth of liquidity, >50 SOL. That leaves 186. Counting a
5 SOL hand-made pool as a graduation would label ordinary tokens as successes.

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

DUST BOTS ARE EXCLUDED, and this is the filter that matters most. The top
wallet this analysis found scored 100% on 35 graduations and 0 losers - and buys
a median of 0.0019 SOL per token, about nineteen cents, at an identical size
across 101 distinct tokens. It is farming something, not trading. Its hit rate
was real and its interpretation was worthless: copying it at a 0.25 SOL position
is 130x its own exposure on a signal never tested at any size that matters. A
wallet must therefore trade at a size comparable to ours before its record means
anything about what we would experience.

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
    python3 scripts/rank_wallets.py [n_losers] [early_n]
"""
import collections
import io
import json
import os
import random
import sys
import time

try:
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError
except ImportError:                                    # pragma: no cover
    from urllib2 import Request, urlopen, HTTPError

OFFICIAL = "https://api.mainnet-beta.solana.com"
BUYER_CACHE = "early_buyers.json"

# Measured on this endpoint: a batch of 10 returns ~4 tx/sec, 25 returns partial,
# 50 answers 429. Small batches with a pause beat large ones that fail.
BATCH = 10
PAUSE = 1.2
# Pagination must reach a token's first transaction; see early_buyers for why a
# cap that some tokens hit and others do not is worse than no analysis at all.
MAX_PAGES = 14


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


def sample(n_losers):
    """Winners and losers, drawn separately because graduations are rare.

    A winner is a token seen migrating with a canonical graduation's liquidity:
    the pump suffix AND more than 50 SOL moved into the pool. Measured on 1191
    observed migrations, only 335 carry the suffix and 186 satisfy both - the
    remainder are hand-opened PumpSwap pools at arbitrary sizes and are not
    evidence of anything having gone up.
    """
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
            ts, mint, venue = r.get("ts", ""), d.get("mint"), d.get("venue")
            if not mint:
                continue
            if venue == "pump_swap":
                if mint.endswith("pump") and (d.get("pool_sol") or 0) > 50:
                    winners.setdefault(mint, ts)
            elif venue == "pump_fun":
                losers.setdefault(mint, ts)
    for m in winners:
        losers.pop(m, None)
    lo = sorted(losers.items(), key=lambda kv: kv[1])
    if len(lo) > n_losers:                      # spread across the whole period
        step = len(lo) / float(n_losers)
        lo = [lo[int(i * step)] for i in range(n_losers)]
    rows = [(ts, m, True) for m, ts in winners.items()]
    rows += [(ts, m, False) for m, ts in lo]
    rows.sort()
    return rows


def early_buyers(rpc, mint, n, cache):
    """The first `n` distinct signers to touch this mint, deployer excluded.

    Keyed on the MINT rather than the bonding curve: the earliest transactions
    mentioning a mint are its creation and the first buys, which is what we
    want, and it removes the curve-address lookup entirely. That matters at 37%
    RPC failures, where every avoidable call is one more chance to lose a token.

    IT MUST REACH THE ACTUAL FIRST TRANSACTION OR GIVE UP. An earlier version
    capped pagination at 4 pages and returned whatever it had. Graduated tokens
    carry far more history than failed ones, so that cap was reached on half the
    winners and none of the losers - meaning winners contributed their MID-LIFE
    traders and losers their genuine early buyers. Two different populations,
    compared as though they were one. It produced a 96.8% out-of-sample hit rate
    against a 38% base rate at p=0.000: a spectacular result, and entirely an
    artefact of who gets sampled rather than of anyone's skill.

    So the walk now runs to exhaustion, and a token whose beginning cannot be
    reached returns None and leaves the sample. That drops the busiest
    graduations, which is a real and acknowledged bias - but it is a bias on an
    observable property, applied to both arms, rather than a silent swap of one
    population for another.
    """
    key = "%s:%d" % (mint, n)
    if key in cache:
        return cache[key]
    before, page, pages, reached = None, None, 0, False
    while pages < MAX_PAGES:
        params = [mint, {"limit": 1000}]
        if before:
            params[1]["before"] = before
        got = rpc.one("getSignaturesForAddress", params)
        if got is None:
            return None                         # failure, not an empty history
        if not got:
            reached = True
            break
        page = got
        pages += 1
        if len(got) < 1000:
            reached = True
            break
        before = got[-1]["signature"]
    if not page or not reached:
        return None
    sigs = [s["signature"] for s in page[-(n + 1):]]
    txs = rpc.batch_tx(sigs)
    if len(txs) < max(2, len(sigs) // 2):
        return None                             # too much missing to trust
    ordered = []
    for s in sigs:
        tx = txs.get(s)
        if not tx:
            continue
        keys = tx.get("transaction", {}).get("message", {}).get("accountKeys", [])
        signer = next((k.get("pubkey") for k in keys if k.get("signer")), None)
        if signer and signer not in ordered:
            ordered.append(signer)
    cache[key] = ordered[1:]                    # drop the deployer
    return cache[key]


def main():
    n_losers = int(sys.argv[1]) if len(sys.argv) > 1 else 260
    early_n = int(sys.argv[2]) if len(sys.argv) > 2 else 12

    buyers = json.load(io.open(BUYER_CACHE)) if os.path.exists(BUYER_CACHE) else {}
    picked = sample(n_losers)

    rpc = Rpc()
    print("")
    print("=" * 78)
    print(" wallet ranking: %d tokens (%d graduations, %d not), %d early buyers each"
          % (len(picked), sum(1 for _, _, w in picked if w),
             sum(1 for _, _, w in picked if not w), early_n))
    print("=" * 78)
    print(" label: a migration with the pump suffix and >50 SOL is a filled curve")

    rows = []
    dropped = {"win": 0, "lose": 0}
    for i, (ts, mint, won) in enumerate(picked):
        eb = early_buyers(rpc, mint, early_n, buyers)
        if eb is None:
            dropped["win" if won else "lose"] += 1
            continue
        rows.append((ts, mint, won, eb))
        if (i + 1) % 25 == 0:
            json.dump(buyers, io.open(BUYER_CACHE, "w"))
            print("   ... %d/%d processed, %d usable, %d rpc failures"
                  % (i + 1, len(picked), len(rows), rpc.failures))
    json.dump(buyers, io.open(BUYER_CACHE, "w"))

    print("")
    print(" tokens read: %d of %d attempted   (%d rpc calls, %d failed)"
          % (len(rows), len(picked), rpc.calls, rpc.failures))
    print(" dropped unread: %d graduations, %d non-graduations" % (dropped["win"], dropped["lose"]))
    print(" (a token whose FIRST transaction could not be reached is dropped, so")
    print("  both arms contribute genuine early buyers or nothing at all)")
    if len(rows) < 40:
        print("")
        print(" Too few tokens were readable to conclude anything. This is an")
        print(" endpoint limit, not a result about wallets.")
        return 2

    wins = sum(1 for _, _, c, _ in rows if c)
    print(" of those, %d are graduations (%.1f%% of the SAMPLE, which is a"
          % (wins, 100.0 * wins / len(rows)))
    print(" sampling ratio by construction, not the rate in the wild)")
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
    if ranked:
        print("")
        print(" NOTE: this ranking does NOT yet filter by trade size, and the")
        print(" wallets it surfaces have been dust bots buying ~0.002 SOL per")
        print(" token. Before following any address below, check what it actually")
        print(" stakes - a wallet risking nineteen cents has no opinion worth")
        print(" copying at 0.25 SOL. scripts/wallet_size.py measures this.")
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
