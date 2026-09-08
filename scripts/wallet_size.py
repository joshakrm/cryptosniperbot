#!/usr/bin/env python3
"""How much does a wallet actually stake per trade?

The wallet ranking surfaced addresses hitting graduations at ~100%, and the top
one buys a median of 0.0019 SOL per token - nineteen cents - at an identical
size across 101 distinct tokens. It is farming something. Its record says
nothing about what a 0.25 SOL position would have experienced, because it has
never taken one.

So before following any address, ask what it risks. A wallet whose median trade
is orders of magnitude below yours is not a trader you are copying; it is a bot
whose incentives you do not share.

Usage:  python3 scripts/wallet_size.py <wallet> [n_transactions]
"""
import json
import sys
import time

try:
    from urllib.request import Request, urlopen
except ImportError:                                   # pragma: no cover
    from urllib2 import Request, urlopen

OFFICIAL = "https://api.mainnet-beta.solana.com"
WSOL = "So11111111111111111111111111111111111111112"
QUOTES = {WSOL, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"}


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


def trade(tx, wallet):
    """(mint, sol, direction) for the wallet's position change, or None."""
    meta = tx.get("meta") or {}
    if meta.get("err") is not None:
        return None

    def bal(key):
        out = []
        for e in (meta.get(key) or []):
            if e.get("owner") != wallet:
                continue
            ui = e.get("uiTokenAmount") or {}
            try:
                amt = float(ui.get("uiAmountString") or 0)
            except (TypeError, ValueError):
                amt = 0.0
            out.append((e.get("mint"), amt))
        return out

    pre, post = bal("preTokenBalances"), bal("postTokenBalances")
    best = None
    for m, a in post:
        if m in QUOTES:
            continue
        b0 = next((x for mm, x in pre if mm == m), 0.0)
        if a - b0 != 0 and (best is None or abs(a - b0) > abs(best[1])):
            best = (m, a - b0)
    if best is None:
        for m, a in pre:
            if m in QUOTES or a <= 0:
                continue
            if any(mm == m for mm, _ in post):
                continue
            if best is None or a > abs(best[1]):
                best = (m, -a)
    if best is None:
        return None
    keys = [k.get("pubkey") for k in
            tx.get("transaction", {}).get("message", {}).get("accountKeys", [])]
    d = 0.0
    for i, k in enumerate(keys):
        if k == wallet:
            d += ((meta.get("postBalances") or [0])[i]
                  - (meta.get("preBalances") or [0])[i]) / 1e9
    return best[0], abs(d), ("buy" if best[1] > 0 else "sell")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    wallet = sys.argv[1]
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 100

    sigs = rpc("getSignaturesForAddress", [wallet, {"limit": n}])
    if not sigs:
        print("could not read this wallet's history - that is an endpoint")
        print("problem, not a fact about the wallet")
        return 2

    buys, sells, mints, failed = [], [], set(), 0
    for i, s in enumerate(sigs):
        tx = rpc("getTransaction", [s["signature"],
                                    {"encoding": "jsonParsed",
                                     "maxSupportedTransactionVersion": 0}])
        time.sleep(0.85)
        if not tx:
            failed += 1
            continue
        t = trade(tx, wallet)
        if not t:
            continue
        mint, sol, side = t
        mints.add(mint)
        (buys if side == "buy" else sells).append(sol)

    print("")
    print(" wallet   %s" % wallet)
    print(" scanned  %d transactions (%d unreadable)" % (len(sigs), failed))
    print(" trades   %d buys, %d sells, across %d distinct tokens"
          % (len(buys), len(sells), len(mints)))
    for label, vals in (("buy", buys), ("sell", sells)):
        if not vals:
            continue
        vals.sort()
        print(" %-8s median %.4f SOL   min %.4f   max %.4f"
              % (label, vals[len(vals) // 2], vals[0], vals[-1]))
    print("")
    if buys:
        med = sorted(buys)[len(buys) // 2]
        if med < 0.02:
            print(" DUST. A median buy of %.4f SOL is not a position, it is a" % med)
            print(" marker. This wallet is farming or indexing, and its hit rate")
            print(" says nothing about what a real trade would have done.")
        elif med < 0.1:
            print(" Small. %.4f SOL is a fraction of a 0.25 SOL position, so this" % med)
            print(" wallet is risking far less than you would be on the same call.")
        else:
            print(" %.4f SOL median buy - comparable to a real position. Its" % med)
            print(" record is at least about the same game you would be playing.")
    if buys and sells and len(sells) < len(buys) / 5.0:
        print("")
        print(" It buys %.0fx more often than it sells, so there is little exit to"
              % (len(buys) / float(max(1, len(sells)))))
        print(" mirror - a full copy would be entries only in practice.")
    print("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
