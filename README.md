# solsnipe

A Solana new-pool sniper in Rust. Watches pump.fun, PumpSwap, Raydium AMM V4 and
Raydium CPMM for token launches, screens each candidate against a fail-closed
safety gauntlet, and trades the survivors.

**Execution is simulated.** Live trading is deliberately not implemented — see
[Live trading](#live-trading).

### Status

Builds clean in WSL (see [Setup](#setup)), `clippy -D warnings` clean, 62 tests
passing, and every regression test mutation-checked (10/10 caught).

**Verified against live mainnet** over three 90-second runs on the public RPC:

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| detections converted to candidates | 18/35 (51%) | 17/21 (81%) | **45/45 (100%)** |
| transaction fetch failures | 18 | 0 | 0 |
| Raydium CPMM false positives | 2 | 0 | 0 |
| PumpSwap candidates | 0 | 0 | **5** |

Each gap between runs was a real bug found by watching live traffic, not by
reading the code: unscoped log markers, too-short fetch retries, and the LP-mint
collision described above.

Also confirmed live: authority checks read correctly on 45/45 candidates, the
Jupiter round trip priced BONK at 4 bps with a depth probe absorbing 5 SOL at
0.3% impact, and the fail-closed contract held — the public RPC returned HTTP
429 mid-screen and the screener **rejected** rather than passing.

**Still unverified:** `holders` / `concentration`. `getTokenLargestAccounts` is
throttled on every free endpoint (429 on mainnet-beta, 403 on publicnode), so it
is currently the *only* thing rejecting candidates and needs your own RPC key to
exercise. Nothing downstream of it — risk gates, paper fills, position
management — has run against real signals yet, because nothing has reached them.

---

## What it actually does

```
websocket logsSubscribe  ->  launch detected
        |
   getTransaction        ->  extract the launched mint
        |
   SCREEN (fail closed)  ->  mint authority, freeze authority, Token-2022
        |                    extensions, holder concentration, buy route,
        |                    SELL ROUTE, round-trip loss, depth probe
        |
   RISK GATES            ->  concurrency, daily loss, trade cap, cooldown,
        |                    kill switch
        |
   PAPER FILL            ->  slippage + latency penalty + fill probability
        |
   POSITION MANAGER      ->  TP ladder / stop / trailing stop / max hold
        |
   journal.jsonl         ->  every decision, including the rejections
```

### Finding the mint without decoding account layouts

Detection deliberately avoids per-venue account layout decoding, which breaks on
every program upgrade. Instead it reads the token balances a transaction
touched. Two rules make that work, and both were derived from live traffic
rather than guessed:

**Markers are scoped to the program that emitted them.** `logsSubscribe` with a
`mentions` filter delivers the *entire* log stream of any transaction touching
the program, including lines from every other program in the call tree. Matching
anywhere in that stream made an arbitrage bot routing through Raydium look like
a brand-new pool. The log is a flat rendering of a call tree, so tracking
`invoke` / `success` recovers who actually spoke.

**The LP mint is separated from the base mint by ownership.** A pool creation
mints LP tokens to the creator *alongside* the base token, so "exactly one
non-quote mint" silently dropped every PumpSwap launch — 4 detected, 0
candidates. At creation the LP mint exists only in the creator's hands, while
the base token is also sitting in the pool vault:

```
So1111…1112   owner=pool-vault    371730970190   (9)   <- quote
GLu7LT…347z   owner=CREATOR       472269607342   (9)   <- LP mint
3YoCWJ…bonk   owner=pool-vault    600000000000   (6)   <- the launch
3YoCWJ…bonk   owner=CREATOR       400000000000   (6)
```

So among several non-quote mints, the one with a holder other than the fee payer
is the launch. A genuine arbitrage transaction has several such mints and is
still rejected — guessing between them is how you end up buying somebody else's
swap leg.

### The screening layer is the point

Anyone can write the part that buys. The part that decides *not* to buy is where
the money is. Every check fails closed: an RPC timeout is a rejection, not a
pass. A false reject costs one missed trade; a false accept costs the position.

| Check | What it catches |
|---|---|
| `mint_authority` | Dev can print unlimited supply and dilute you to zero |
| `freeze_authority` | Dev can freeze your token account so you never sell |
| `token2022_extensions` | Transfer hooks, transfer fees, permanent delegate — post-purchase honeypots |
| `holder_count` | Nobody is actually in this token |
| `concentration` | Top holders can exit into you at will |
| `buy_route` | Not tradable yet |
| **`sell_route`** | **You can buy but not exit. This is the honeypot test.** |
| `roundtrip` | Fees and spread eat you alive before any price move |
| `depth` | Pool cannot absorb your size without extreme impact |
| **`lp_burned`** | **The creator can withdraw the pool out from under you** |

A check has three outcomes, not two. **Failed** means it ran and the answer
disqualified the token. **Unavailable** means it could not run at all — an RPC
error, a timeout, throttling. Both block the trade by default, but they demand
opposite responses: the first says screening is working, the second says your
endpoint is broken. Folding them together makes a throttled RPC look like a wall
of dangerous tokens, which is exactly what a live run on the public RPC looked
like before the distinction existed. `stats` reports them separately, and
`screen.treat_unavailable_as` controls the policy (default: reject).

`sell_route` and `roundtrip` are the highest-value checks here. Authority flags
tell you what the dev *could* do; a live round-trip quote tells you what the
market will *actually* let you do right now.

Both legs are quoted at `risk.position_size_sol` — the size you actually trade.
Price impact is size-dependent, so pricing an entry off a smaller sample
understates the real fill and makes every later `gain_bps` optimistic.

### Exit rules

| Rule | Behaviour |
|---|---|
| Take-profit ladder | Each rung sells a share of the **original** position size, so a ladder summing to 100 fully exits. At most one rung per sweep, so a vertical candle cannot dump the position into its own spike. |
| Stop loss | Checked first — it is the one rule whose entire job is to be fast. |
| Trailing stop | Arms only once the peak clears entry by at least the trail width. Otherwise a 25% trail fires *below* entry and silently overrides your 35% stop with a tighter one. |
| Max hold | Hard timeout, regardless of PnL. |
| Unroutable | Three consecutive sweeps with no sell route before a position is written off. One failed quote is equally consistent with an aggregator hiccup, and booking a total loss on that turns someone else's outage into your realised loss. |
| Dust | Measured in SOL **value**, not token count. After a 100x, 0.1% of the tokens is still a tenth of the stake. |

A transport error while marking is treated as *no information* and never acts —
only an actual "no route" answer counts against a position.

---

## Setup

> **This builds in WSL, not on Windows.** Windows 11 **Smart App Control** is
> enabled on this machine (`VerifiedAndReputablePolicyState = 1`). It blocks
> execution of unsigned binaries, which is every build script, proc-macro, and
> output binary that Rust produces — builds die with `os error 4551`. Smart App
> Control cannot be re-enabled once turned off without reinstalling Windows, so
> building in WSL is the sane path. It costs nothing here: this bot only talks
> websockets and HTTP.

### 1. Prerequisites (already done on this machine)

```bash
wsl -d Ubuntu -u root -- apt-get install -y build-essential pkg-config libssl-dev
```

`libssl-dev` matters: `native-tls` resolves to SChannel on Windows but to
**OpenSSL on Linux**, so the Linux build needs the headers.

### 2. Install Rust in WSL

```bash
wsl -d Ubuntu -- bash -c "curl -sSfL https://sh.rustup.rs | sh -s -- -y"
```

### 3. Configure

```bash
cp /mnt/c/Users/joshr/Documents/solsnipe/config.example.toml /mnt/c/Users/joshr/Documents/solsnipe/config.toml
```

Edit `config.toml` and set `rpc.http_url` and `rpc.ws_url`. **A public RPC will
rate-limit you into uselessness** — the free tier at Helius, QuickNode, or Triton
is the minimum viable setup. The program refuses to start while the `YOUR_KEY`
placeholder is still there.

`config.toml` is gitignored. Keep it that way.

### 4. Build

The source lives on the Windows filesystem so you can edit it from either side,
but `target/` goes in the Linux filesystem — that is where the build I/O churn
is, and `/mnt/c` is slow across the 9p bridge.

```bash
wsl -d Ubuntu -- bash /mnt/c/Users/joshr/Documents/solsnipe/scripts/wsl-cargo.sh build --release
```

Run that from **PowerShell or cmd, not Git Bash** — Git Bash rewrites `/mnt/c/...`
into a Windows path before `wsl.exe` sees it, producing a baffling
`C:/Program Files/Git/mnt/c/...: No such file or directory`. Prefix with
`MSYS_NO_PATHCONV=1` if you must use Git Bash.

`scripts/wsl-cargo.sh` sets `CARGO_TARGET_DIR` and forwards whatever you pass to
cargo. It exists because calling cargo through `wsl.exe -- bash -c "..."` from a
Windows shell is a quoting minefield: `$HOME` expands on the Windows side before
WSL sees it, and the inherited Windows PATH contains unescaped parentheses
(`Program Files (x86)`) that break bash parsing outright.

Working inside WSL directly, add this to `~/.bashrc` and just use cargo normally:

```bash
export CARGO_TARGET_DIR=$HOME/.cargo-target/solsnipe
```

## Usage

Run everything from inside WSL (`wsl -d Ubuntu`, then `cd /mnt/c/Users/joshr/Documents/solsnipe`).

Screen a single mint — the fastest way to sanity-check your thresholds against
tokens whose outcome you already know:

```bash
cargo run --release -- screen <MINT_ADDRESS> --venue pump_fun
```

Pass the venue you are actually testing. Concentration is advisory on a pump.fun
bonding curve and fatal everywhere else, so screening as `unknown` can disagree
with the verdict the live path would reach.

Run the paper sniper:

```bash
cargo run --release -- run
```

Summarise a session:

```bash
cargo run --release -- stats
```

Turn up the logging:

```bash
RUST_LOG=solsnipe=debug cargo run --release -- run
```

### Kill switch

Create a file named `KILL` in the working directory. Entries stop immediately
and open positions are flattened on the next sweep. Delete it to resume.

```bash
touch /mnt/c/Users/joshr/Documents/solsnipe/KILL
```

It is a file rather than a signal handler on purpose: you can stop the bot from
any terminal, any script, or a phone over SSH, without finding the process.

---

## Reading your paper results honestly

The failure mode of every home-made sniper backtest is assuming you were first.
You were not. Three knobs in `[paper]` model that, and they are the difference
between a strategy and a fantasy:

- **`latency_penalty_bps`** — how far the price ran before your transaction
  landed. Defaults to 1200 (12%). Setting this near zero produces a beautiful,
  meaningless equity curve.
- **`fill_probability`** — the fraction of races you lose outright to faster
  bots. Defaults to 0.55.
- **`slippage_bps`** — the spread you actually cross.

Set these from your journal, not from hope. The honest test: **if your paper
results only work with `latency_penalty_bps` near zero, you do not have a
strategy — you have a latency problem you have not solved yet.**

What to look at in `stats`:

- **rejections by check** — if one check rejects almost everything, either the
  threshold is wrong or that check is doing all the work. Both are worth knowing.
- **races lost (nofill)** — your realistic ceiling on trade count.
- **win rate vs net PnL** — sniping is usually a low-win-rate, fat-tail game. A
  60% win rate with negative PnL means your losers are too big; tighten the stop.
  A 20% win rate with positive PnL is normal and fine.

---

## Development

```bash
wsl -d Ubuntu -- bash scripts/wsl-cargo.sh test
wsl -d Ubuntu -- bash scripts/wsl-cargo.sh clippy --all-targets -- -D warnings
```

The exit rules live in a pure function, `decide_exit` in
[src/position.rs](src/position.rs), deliberately separated from execution: these
are the rules that decide when real money leaves a position, so they are worth
testing without a network, an executor, or a wall clock.

Marks come through a `PriceSource` trait rather than a direct Jupiter call, for
the same reason. That lets the integration tests drive a whole position through
mark -> decide -> sell -> accounting with a scripted price series, and assert on
exact PnL and on the risk slot being released. Those paths are otherwise
unreachable without a paid RPC endpoint, since `holders` rejects every candidate
before anything downstream ever runs.

`scripts/smoke.sh` exercises the CLI and, more usefully, every configuration
**refusal** — the point of a fail-closed design is that it actually refuses.

`scripts/mutation_check.py` reintroduces fixed bugs one at a time and asserts the
matching test fails. A regression test that still passes against the bug it
names is worthless, and this catches that. It currently covers **10 bugs that
were genuinely shipped in this repo**, all 10 caught.

It earned its keep immediately: the trailing-stop test as first written passed
against the bug it claimed to cover, and only became a real test once its price
was tightened. Without the mutation check that would have read as green.

```bash
wsl -d Ubuntu -- bash -lc "cd /mnt/c/Users/joshr/Documents/solsnipe && python3 scripts/mutation_check.py"
```

## What live traffic actually showed

Numbers measured against mainnet, not estimated. They drive most of the defaults
in `config.example.toml`, and none of them are recoverable by reading the code.

### The token index lags launches by ~13 seconds

Over 73 live launches, Helius answered `getAccountInfo` instantly but reported
`Invalid param: not a Token mint` from `getTokenLargestAccounts` for a **median
of 13.0s** (max 16.3s) after the launch transaction.

That single fact reshapes the strategy. Screen any sooner and the holder checks
cannot run at all — an early build rejected 96% of candidates as `unavailable`
and looked, wrongly, like a wall of dangerous tokens. Hence
`min_pool_age_ms = 15000`, plus a retry in `rpc.rs` for the tail.

The consequence is worth stating plainly: **this is not a first-block sniper.**
It enters ~15s after launch. On a home connection you were never going to win a
block race against colocated bots anyway, so the honest play is to spend that
time buying information instead.

### Holder counts are bimodal

Once indexed, launches cluster at **1–3 holders** or at **10–20**, with almost
nothing between:

```
 1 holder  ████████████████████████  24
 2         █████████████             13
 3         █████████                  9
 4-9       ██████                     6
10-20      ████████████████          21
```

So `min_holders` is a coarse switch, not a dial. Moving it 10 → 5 changes the
pass rate only 20% → 25%, because 5 sits in the empty middle. The meaningful
settings are ~10 (only tokens with traction) or ~2 (nearly everything).

### One PumpSwap pool in four has withdrawable liquidity

Extracting the LP mint from the launch transaction (the non-quote mint held only
by the creator, the same discriminator that separates it from the base token)
and reading its supply:

```
3 pools   LP supply 0            burned, liquidity locked
1 pool    4.1e12 units outstanding   the creator can withdraw the pool
2 pools   no LP mint at all       bonding curve, nothing to withdraw
```

Every other check in this repo passes that fourth pool without comment: the
token is fine, the authorities are renounced, it is sellable right now. The rug
does not touch the token at all — it removes the money. That is what `lp_burned`
catches, and it is why it was worth building when metadata mutability was not.

### The aggregator is the real rate limit

Position marks vastly outnumber screening quotes — six positions polled every
few seconds is ~2 req/s against ~0.4 req/s of screening — so marks starve
screening and candidates come back unscreenable through no fault of their own.
At six concurrent positions, **33% of screens were degraded by HTTP 429**.

All Jupiter traffic is serialised through one shared budget so requests queue
instead of failing, and marks wait a multiple of the screening interval so they
stand aside. Queueing beats firing and failing, because a 429 carries no
information about the token.

The interval is **adaptive rather than configured**, and that turned out to
matter. Hand-tuning it was guessing at an undocumented limit that differs per
endpoint and plan: a flat 400ms still lost 21% of screens. It now backs off
multiplicatively on every 429 and creeps back while requests land cleanly, so it
finds the sustainable rate itself and re-finds it when conditions change. A
throttled request is also retried once, so being throttled costs a delay rather
than the whole candidate. The heartbeat reports the interval it settled on — a
value pinned at the ceiling means the endpoint is your bottleneck.

### A stop loss sets the trigger, not the fill

The first closed trade stopped out at −72% against a −35% stop. Nothing
malfunctioned: a fresh pump.fun token can halve between sweeps. Tightening
`poll_interval_ms` narrows that gap but spends Jupiter budget, which costs you
screening. That trade-off is real and unavoidable on a free tier.

### A position you cannot mark is a position you cannot manage

The worst bug found so far, and it only appeared over a full hour.

`Mark::Unknown` (a transport failure) correctly refuses to fire any price-based
rule — but it used to skip `evaluate()` altogether, so the **clock-based** rule
never ran either. A position whose marks kept failing was held forever.

Measured: 10 positions open for 58 minutes against a 15-minute `max_hold_secs`.
And it cascaded. Those positions kept consuming mark budget indefinitely, which
starved screening, which took approvals from 16 in the first ten minutes to
**zero across the following 1,672 screens**:

```
 +0-10m   screened 329   approved 16   degraded 20%
+10-20m   screened 293   approved  0   degraded 26%
...
+40-50m   screened 386   approved  0   degraded 35%
```

An hour of runtime produced 14 entries, all in the first 140 seconds.

Positions past their hold window that still cannot be marked are now closed at
the last price actually observed — a stale valuation, recorded as such, but the
alternative is holding a risk slot and burning budget forever.

Worth recording how this was found: the adversarial review raised it and the
verify stage **refuted** it. The refutation was wrong and the live traffic
settled it. Adversarial verification cuts false positives, and it will
occasionally cut a true one — a reason to keep running the thing against reality
rather than treating a clean review as proof.

### Two settings that must agree, and nothing was checking

Screening makes up to three Jupiter quotes, and the adaptive throttle may space
them as widely as `jupiter_max_interval_ms`. If `screen.max_screen_ms` is
narrower than that product, screening times out on exactly the candidates that
got far enough to reach routing.

Measured: an 8s budget against a 4s ceiling produced 40-60 `screen_timeout`
rejections per five minutes. Every individual number looked healthy - the
throttle was correctly backing off to avoid 429s, the endpoint was fine, the
median screen took 385ms - and approvals still went to nearly zero. The throttle
protecting itself from rate limits pushed screening past its own deadline.

`Config::validate` now refuses to start on that combination and says which value
to change. It is the kind of interlock that no single check can see and that
costs a whole run to discover empirically.

### The holder check was doing two jobs, and only one was safety

Measured across both configurations:

| | rejected by a cheap check | reached routing |
|---|---|---|
| holders on | **79%** (1,700 of 2,154) | 14% |
| holders off | **3%** (57 of 1,749) | 8% |

One RPC call was turning away four candidates in five before any of them reached
the three aggregator quotes that actually cost budget. Remove it and everything
goes to routing, which is ~1.5 req/s against a free-tier ceiling near 1 — hence
89% of screens timing out.

The checks that survive at t=0 cannot replace it. `lp_burned` rejects 3%, because
most pump.fun launches are bonding curves with no LP mint to examine. The
authority checks fired **once each in 2,154 screens**: pump.fun renounces mint
and freeze authority by construction, so they are real rug protection and useless
as a filter.

So a fast-entry configuration needs a filter that works at t=0 and costs nothing.
`extract_launch_metrics` records two candidates for the role — creator share of
supply, and SOL placed in the pool — both read out of the launch transaction that
is fetched anyway. They are **recorded, not enforced**: a threshold should come
from a measured distribution, the way the exit thresholds should have.

### Running on free infrastructure

No API key, no signup, no account: `solana-rpc.publicnode.com` serves
`getTransaction`, `getAccountInfo` and `getTokenSupply` at ~5 calls/sec with
websockets, roughly five times what a spent Helius free tier delivered.

The one method it refuses is `getTokenLargestAccounts` — which feeds the holder
checks, which need the 15s index wait, which does not work. The free tier and the
only viable strategy want the same configuration. `config-free.toml` is it.

Everything else tested was unusable: Ankr, Alchemy demo and OnFinality returned
403 or 429 immediately; dRPC and OMNIA returned 400 and 521.

### The one filter that costs nothing

With the holder checks gone, the aggregator budget collapses — 63% of screens
timed out because every candidate went straight to three quotes. The fix had to
be a filter that works at t=0 and costs nothing, and the launch transaction was
already carrying one:

```
SOL in pool at launch (n=225):  median 0.575   p90 6.9   max 850
   reject < 2.0 SOL  drops 62%
```

`min_pool_sol` runs as check zero, before any network call, reading a number the
transaction already contained. Measured effect:

| | without | with |
|---|---|---|
| screen timeouts | 63% | **9%** |
| median screen time | 15,000ms (timing out) | **0ms** |
| rejected at zero cost | 0% | 63% |

It is also economically honest rather than an arbitrary throughput hack: a pool
this thin fails the `depth` check later anyway, after spending the quotes the
gate exists to save.

### Read mid-run PnL with suspicion

Losers hit their stop within seconds; winners need minutes to reach a
take-profit rung or `max_hold_secs`. Any PnL sampled mid-run is therefore drawn
from the worst end of the distribution. `scripts/report.py` excludes open
positions and says so rather than quietly averaging them in.

## Known gaps

Stated plainly, because a sniper you do not understand is a sniper that will
surprise you:

1. **Log markers drift.** Detection keys off strings the programs emit
   (`initialize2`, `Instruction: Create`). Program upgrades change these. If
   detections dry up, `src/decode.rs::is_launch_log` is the first place to look.
   Markers are matched only when the target program is the innermost executing
   program — `logsSubscribe` delivers the *whole* log stream of any transaction
   that touches the program, so unscoped matching reads an arbitrage bot routing
   through Raydium as a brand-new pool. That was observed live, not theorised.
2. **Program IDs are unverified.** The ones in `config.example.toml` are the
   widely-published values. Verify them on Solscan before trusting them.
3. **Every pump.fun mint is Token-2022.** Confirmed on live traffic: 18/18
   candidates in one run. This is why `token2022_extensions` rejects specific
   *hostile* extensions rather than the token program itself — writing that
   check as "reject Token-2022" would silently reject the entire venue while
   looking like it was working.
4. **LP locked in a third-party locker reads as unlocked.** `lp_burned` keys
   off the LP mint's supply, so a pool whose LP is locked rather than burned
   reports a non-zero supply and is rejected. That is the fail-closed direction
   and it is deliberate: verifying a locker would mean resolving every holder's
   owner, and the set of locker programs is not enumerable anyway.
5. **No metadata mutability check — deliberately, and measured.** DAS
   `getAsset` exposes `mutable` for one extra call with no `solana-sdk`, so the
   cost was never the obstacle. The obstacle is that it carries no signal:
   **25 of 25 sampled live mints reported `mutable: true`**, across pump.fun and
   PumpSwap alike. A check that cannot ever reject anything is not a safety
   check, it is a per-candidate RPC call that makes the report look thorough.
   It would also be the wrong tool regardless — mutable metadata lets a dev
   change the name and picture, which matters to a human reading a token page
   and not at all to a bot buying on mechanical criteria.
6. **Holder concentration is weak on fresh pump.fun launches.** The bonding
   curve legitimately holds nearly all supply at t=0, so the check is advisory
   on that venue rather than fatal.
7. **`getTokenLargestAccounts` caps at 20 entries**, so `holder_count` is a
   floor, not the true count.
8. **Jupiter quotes are not fills.** A route existing in a quote does not
   guarantee the transaction lands.

---

## Live trading

`live.enabled = true` makes the program **refuse to start**, rather than
silently pretend to trade. This is intentional.

`src/exec/live.rs` documents what a real implementation needs. The step people
underestimate is confirmation reconciliation: when a signature confirmation
times out but the trade actually landed, a naive bot assumes no fill, re-enters,
and ends up double-sized in a token that is already dumping.

Before wiring any of that up, get a few hundred journalled signals first. If the
paper numbers do not work with honest latency assumptions, live numbers will be
worse, not better.

---

## Risk

This trades the most adversarial corner of crypto. Most new token launches go to
zero, many are engineered specifically to take money from bots exactly like this
one, and the screening here reduces that exposure without eliminating it. Losing
the entire balance is a normal outcome, not an edge case.

Not financial advice. Your keys, your machine, your money, your call.
