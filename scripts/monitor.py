#!/usr/bin/env python3
"""Live dashboard for a running solsnipe.

Reads the journal file and serves it as a self-refreshing page. It never touches
the bot: no shared state, no port the bot listens on, nothing it can break. Kill
it, restart it, or point it at a finished journal - the bot does not notice.

Usage:
    python3 scripts/monitor.py [journal.jsonl] [port]

Then open http://localhost:8712
"""
import collections
import io
import json
import os
import re
import sys
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer

JOURNAL = sys.argv[1] if len(sys.argv) > 1 else "journal.jsonl"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8712

# A journal that has not grown in this long means the bot is not running.
# The distinction matters: a quiet bot and a dead bot look identical otherwise,
# and "nothing is happening" is exactly when you want to know which it is.
STALE_AFTER = 90


def parse_ts(s):
    s = re.sub(r"\.(\d{6})\d+", r".\1", (s or "").replace("Z", "+00:00"))
    try:
        return datetime.fromisoformat(s)
    except ValueError:
        return None


def load(path):
    if not os.path.exists(path):
        return []
    rows = []
    for line in io.open(path, encoding="utf-8", errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except ValueError:
            continue  # a half-written final line while the bot appends
    return rows


def build_state(path):
    rows = load(path)
    now = datetime.now(timezone.utc)

    kinds = collections.Counter(r.get("kind") for r in rows)
    screens = [r["data"] for r in rows if r.get("kind") == "screen"]
    opens = {}
    for r in rows:
        if r.get("kind") == "position_open":
            opens[r["data"]["mint"]] = (r["data"], parse_ts(r.get("ts")))
    closes = [(r["data"], parse_ts(r.get("ts"))) for r in rows if r.get("kind") == "position_close"]
    closed_mints = set(d["mint"] for d, _ in closes)

    def blocking(s):
        return [c for c in s.get("checks", [])
                if not c.get("passed") and c.get("severity") != "advisory"]

    approved = sum(1 for s in screens if not blocking(s))

    fatal = collections.Counter()
    unavail = collections.Counter()
    for s in screens:
        for c in blocking(s):
            if c.get("severity") == "unavailable":
                unavail[c["name"]] += 1
            else:
                fatal[c["name"]] += 1

    open_positions = []
    for mint, (d, t) in opens.items():
        if mint in closed_mints:
            continue
        age = int((now - t).total_seconds()) if t else 0
        open_positions.append({
            "mint": mint,
            "venue": d.get("venue") or "",
            "invested": d.get("sol_invested") or 0.0,
            "age_s": age,
        })
    open_positions.sort(key=lambda p: -p["age_s"])

    pnls = [d.get("pnl_sol", 0.0) for d, _ in closes]
    wins = [p for p in pnls if p > 0]
    losses = [p for p in pnls if p <= 0]

    feed = []
    for r in rows[-500:]:
        k = r.get("kind")
        d = r.get("data", {}) or {}
        t = parse_ts(r.get("ts"))
        when = t.strftime("%H:%M:%S") if t else ""
        mint = (d.get("mint") or "")[:16]
        if k == "fill_buy":
            feed.append([when, "entry", "%s   %.4f SOL" % (mint, d.get("sol_amount", 0))])
        elif k == "position_close":
            kind = "win" if d.get("pnl_sol", 0) > 0 else "loss"
            feed.append([when, kind, "%s   %+.4f SOL   %s" % (mint, d.get("pnl_sol", 0), d.get("reason"))])
        elif k == "fill_sell":
            feed.append([when, "partial", "%s   %+.4f SOL   %s" % (mint, d.get("sol", 0), d.get("reason"))])
        elif k == "risk_block":
            feed.append([when, "blocked", str(d.get("reason", ""))[:64]])
    feed.reverse()

    mtime = os.path.getmtime(path) if os.path.exists(path) else None
    quiet = (time.time() - mtime) if mtime else None

    return {
        "journal": os.path.basename(path),
        "exists": os.path.exists(path),
        "live": bool(quiet is not None and quiet < STALE_AFTER),
        "quiet_s": int(quiet) if quiet is not None else None,
        "counts": {
            "candidates": kinds.get("candidate", 0),
            "screened": len(screens),
            "approved": approved,
            "entered": kinds.get("fill_buy", 0),
            "open": len(open_positions),
            "closed": len(closes),
        },
        "rejected": fatal.most_common(8),
        "unavailable": unavail.most_common(6),
        "open_positions": open_positions[:20],
        "pnl": {
            "net": sum(pnls),
            "n": len(pnls),
            "wins": len(wins),
            "losses": len(losses),
            "avg_win": (sum(wins) / len(wins)) if wins else 0.0,
            "avg_loss": (sum(losses) / len(losses)) if losses else 0.0,
        },
        "feed": feed[:40],
    }


PAGE = """<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>solsnipe monitor</title>
<style>
  :root{
    --bg:#0F1319; --panel:#171C24; --panel2:#1E242E; --ink:#E6E9EF; --soft:#B4BCCA;
    --muted:#838C9C; --rule:#2A313D; --amber:#D9A441; --loss:#D97166; --gain:#5FA98A;
  }
  @media (prefers-color-scheme: light){
    :root{ --bg:#FAFAF8; --panel:#FFF; --panel2:#F2F3F0; --ink:#1C2028; --soft:#454C59;
           --muted:#6B7280; --rule:#DEDFDA; --amber:#B07B18; --loss:#B0453A; --gain:#3D7A62; }
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--ink);
       font:14px/1.5 ui-sans-serif,system-ui,sans-serif;-webkit-font-smoothing:antialiased}
  .wrap{max-width:1080px;margin:0 auto;padding:22px 20px 60px}
  header{display:flex;align-items:baseline;gap:14px;flex-wrap:wrap;
         border-bottom:1px solid var(--rule);padding-bottom:14px;margin-bottom:20px}
  h1{font-size:17px;margin:0;letter-spacing:-.01em}
  .dot{display:inline-block;width:8px;height:8px;border-radius:50%;margin-right:7px;vertical-align:1px}
  .live .dot{background:var(--gain);box-shadow:0 0 0 3px color-mix(in srgb,var(--gain) 25%,transparent)}
  .dead .dot{background:var(--loss)}
  .status{font:12px ui-monospace,monospace;letter-spacing:.05em;text-transform:uppercase}
  .live{color:var(--gain)} .dead{color:var(--loss)}
  .meta{margin-left:auto;font:12px ui-monospace,monospace;color:var(--muted)}
  .grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(132px,1fr));gap:1px;
        background:var(--rule);border:1px solid var(--rule);border-radius:3px;overflow:hidden;margin-bottom:22px}
  .cell{background:var(--panel);padding:13px 15px}
  .k{font:10.5px ui-monospace,monospace;letter-spacing:.11em;text-transform:uppercase;color:var(--muted);display:block;margin-bottom:5px}
  .v{font:600 22px ui-monospace,monospace;font-variant-numeric:tabular-nums;display:block;line-height:1.1}
  .v.loss{color:var(--loss)} .v.gain{color:var(--gain)} .v.amber{color:var(--amber)}
  .cols{display:grid;grid-template-columns:1fr 1fr;gap:20px}
  @media(max-width:820px){.cols{grid-template-columns:1fr}}
  section{background:var(--panel);border:1px solid var(--rule);border-radius:3px;padding:15px 17px;margin-bottom:18px}
  h2{font:11px ui-monospace,monospace;letter-spacing:.12em;text-transform:uppercase;
     color:var(--muted);margin:0 0 11px;font-weight:500}
  table{width:100%;border-collapse:collapse;font:12.5px ui-monospace,monospace;font-variant-numeric:tabular-nums}
  td{padding:5px 0;border-bottom:1px solid var(--panel2)}
  tr:last-child td{border-bottom:none}
  td.n{text-align:right;color:var(--soft)}
  .tag{font:10px ui-monospace,monospace;letter-spacing:.06em;text-transform:uppercase;
       padding:2px 6px;border-radius:2px;margin-right:8px}
  .t-entry{background:color-mix(in srgb,var(--amber) 18%,transparent);color:var(--amber)}
  .t-win{background:color-mix(in srgb,var(--gain) 18%,transparent);color:var(--gain)}
  .t-loss{background:color-mix(in srgb,var(--loss) 18%,transparent);color:var(--loss)}
  .t-partial,.t-blocked{background:var(--panel2);color:var(--muted)}
  .empty{color:var(--muted);font-style:italic;font-size:13px}
  .bar{height:5px;background:var(--panel2);border-radius:2px;overflow:hidden;margin-top:4px}
  .bar span{display:block;height:100%;background:var(--amber);opacity:.8}
</style></head>
<body><div class="wrap">
  <header>
    <h1>solsnipe</h1>
    <span id="status" class="status dead"><span class="dot"></span>connecting</span>
    <span class="meta" id="meta"></span>
  </header>
  <div class="grid" id="counts"></div>
  <div class="cols">
    <div>
      <section><h2>Open positions</h2><div id="open"></div></section>
      <section><h2>Closed trades</h2><div id="pnl"></div></section>
    </div>
    <div>
      <section><h2>Why candidates were rejected</h2><div id="rej"></div></section>
      <section><h2>Activity</h2><div id="feed"></div></section>
    </div>
  </div>
</div>
<script>
const fmt = (n,d=4) => (n>=0?"+":"") + n.toFixed(d);
const dur = s => s<60 ? s+"s" : (s<3600 ? Math.floor(s/60)+"m "+(s%60)+"s" : Math.floor(s/3600)+"h "+Math.floor((s%3600)/60)+"m");

function rows(items, render){
  if(!items.length) return '<div class="empty">nothing yet</div>';
  return '<table>' + items.map(render).join('') + '</table>';
}

async function tick(){
  let s;
  try { s = await (await fetch('/api/state')).json(); }
  catch(e){ document.getElementById('status').className='status dead';
            document.getElementById('status').innerHTML='<span class="dot"></span>monitor offline'; return; }

  const st = document.getElementById('status');
  st.className = 'status ' + (s.live ? 'live' : 'dead');
  st.innerHTML = '<span class="dot"></span>' + (s.live ? 'running' : (s.exists ? 'stopped' : 'no journal'));
  document.getElementById('meta').textContent =
    s.journal + (s.quiet_s!==null ? '  ·  last wrote ' + dur(s.quiet_s) + ' ago' : '');

  const c = s.counts;
  document.getElementById('counts').innerHTML = [
    ['launches', c.candidates, ''], ['screened', c.screened, ''],
    ['approved', c.approved, 'amber'], ['entered', c.entered, 'amber'],
    ['open', c.open, ''], ['closed', c.closed, '']
  ].map(([k,v,cls]) => `<div class="cell"><span class="k">${k}</span><span class="v ${cls}">${v}</span></div>`).join('');

  document.getElementById('open').innerHTML = rows(s.open_positions, p =>
    `<tr><td>${p.mint}</td><td class="n">${p.venue}</td><td class="n">${dur(p.age_s)}</td></tr>`);

  const p = s.pnl;
  document.getElementById('pnl').innerHTML = p.n === 0
    ? '<div class="empty">no closed trades yet</div>'
    : `<table>
        <tr><td>net</td><td class="n" style="color:var(--${p.net>=0?'gain':'loss'})">${fmt(p.net)} SOL</td></tr>
        <tr><td>trades</td><td class="n">${p.n}</td></tr>
        <tr><td>win rate</td><td class="n">${(100*p.wins/p.n).toFixed(1)}% (${p.wins}/${p.n})</td></tr>
        <tr><td>avg win</td><td class="n">${fmt(p.avg_win)}</td></tr>
        <tr><td>avg loss</td><td class="n">${fmt(p.avg_loss)}</td></tr>
      </table>`;

  const maxr = Math.max(1, ...s.rejected.map(r=>r[1]), ...s.unavailable.map(r=>r[1]));
  let rej = rows(s.rejected, r =>
    `<tr><td>${r[0]}<div class="bar"><span style="width:${100*r[1]/maxr}%"></span></div></td><td class="n">${r[1]}</td></tr>`);
  if(s.unavailable.length){
    rej += '<h2 style="margin-top:14px">Could not check — your endpoint</h2>' + rows(s.unavailable, r =>
      `<tr><td>${r[0]}</td><td class="n">${r[1]}</td></tr>`);
  }
  document.getElementById('rej').innerHTML = rej;

  document.getElementById('feed').innerHTML = rows(s.feed, f =>
    `<tr><td><span class="tag t-${f[1]}">${f[1]}</span>${f[2]}</td><td class="n">${f[0]}</td></tr>`);
}
tick(); setInterval(tick, 2000);
</script></body></html>
"""


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/api/state"):
            body = json.dumps(build_state(JOURNAL)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
        else:
            body = PAGE.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass  # a request log every 2 seconds is noise, not information


if __name__ == "__main__":
    print("solsnipe monitor")
    print("  journal: %s" % os.path.abspath(JOURNAL))
    print("  open:    http://localhost:%d" % PORT)
    HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
