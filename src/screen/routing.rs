use tracing::debug;

use super::MintFacts;
use crate::config::ScreenConfig;
use crate::rpc::{raw_to_ui, Jupiter, Priority};
use crate::types::{CheckResult, Pubkey, LAMPORTS_PER_SOL};

/// Price impact above this on the depth probe means the pool cannot absorb the
/// size we claim to require, whatever the nominal liquidity says.
const MAX_DEPTH_PROBE_IMPACT: f64 = 0.50;

/// Routing, depth, and the honeypot round trip.
///
/// The round trip is the single most valuable check in this whole program.
/// Authority flags tell you what the dev *could* do; the round trip tells you
/// what the market will *actually* let you do right now. If a live aggregator
/// cannot construct a sell route for tokens you just bought, you are looking at
/// a honeypot and no amount of clean metadata changes that.
///
/// Both legs are quoted at `entry_size_sol` - the size actually traded - rather
/// than at some smaller sample. Price impact is size-dependent, so pricing the
/// entry off a tiny probe understates the real fill and makes every downstream
/// gain_bps measurement optimistic.
///
/// Returns (checks, roundtrip_loss_bps, entry_price_sol_per_token).
pub async fn check(
    jup: &Jupiter,
    cfg: &ScreenConfig,
    entry_size_sol: f64,
    wsol: &str,
    mint: &Pubkey,
    facts: &MintFacts,
) -> (Vec<CheckResult>, Option<i64>, Option<f64>) {
    let mut out = Vec::new();

    let entry_lamports = (entry_size_sol * LAMPORTS_PER_SOL) as u64;
    if entry_lamports == 0 {
        out.push(CheckResult::fail("routing", "entry size is zero"));
        return (out, None, None);
    }

    // --- buy leg, at the size we would really trade ----------------------
    let buy = match jup.quote(wsol, mint, entry_lamports, 500, Priority::Screening).await {
        Ok(Some(q)) => q,
        Ok(None) => {
            out.push(CheckResult::fail(
                "buy_route",
                "no buy route exists - not tradable yet",
            ));
            return (out, None, None);
        }
        Err(e) => {
            out.push(CheckResult::unavailable(
                "buy_route",
                format!("quote failed: {e}"),
            ));
            return (out, None, None);
        }
    };

    let tokens_ui = raw_to_ui(buy.out_amount, facts.decimals);
    if tokens_ui <= 0.0 {
        out.push(CheckResult::fail("buy_route", "buy quote returns zero tokens"));
        return (out, None, None);
    }

    // Measured against what Jupiter actually consumed, not what we asked for.
    let sol_in = buy.in_amount as f64 / LAMPORTS_PER_SOL;
    if sol_in <= 0.0 {
        out.push(CheckResult::fail("buy_route", "buy quote consumed zero SOL"));
        return (out, None, None);
    }
    let entry_price = sol_in / tokens_ui;

    let ext_note = if facts.is_token_2022 { ", token-2022" } else { "" };
    out.push(CheckResult::pass(
        "buy_route",
        format!(
            "{} hop(s), impact {:.2}% at {:.3} SOL{}",
            buy.route_hops,
            buy.price_impact_pct * 100.0,
            sol_in,
            ext_note
        ),
    ));

    // --- sell leg: the honeypot test -------------------------------------
    let sell = match jup.quote(mint, wsol, buy.out_amount, 500, Priority::Screening).await {
        Ok(Some(q)) => q,
        Ok(None) => {
            out.push(CheckResult::fail(
                "sell_route",
                "NO SELL ROUTE - you could buy but not exit. Honeypot.",
            ));
            return (out, None, Some(entry_price));
        }
        Err(e) => {
            out.push(CheckResult::unavailable(
                "sell_route",
                format!("sell quote failed: {e}"),
            ));
            return (out, None, Some(entry_price));
        }
    };
    out.push(CheckResult::pass(
        "sell_route",
        format!("{} hop(s) back to SOL", sell.route_hops),
    ));

    let sol_back = sell.out_amount as f64 / LAMPORTS_PER_SOL;
    let loss_bps = (((sol_in - sol_back) / sol_in) * 10_000.0) as i64;
    debug!(%mint, sol_in, sol_back, loss_bps, "round trip");

    let detail =
        format!("{sol_in:.4} SOL in, {sol_back:.4} SOL back, {loss_bps} bps round-trip loss");
    if loss_bps <= cfg.max_roundtrip_loss_bps as i64 {
        out.push(CheckResult::pass("roundtrip", detail));
    } else {
        out.push(CheckResult::fail(
            "roundtrip",
            format!("{detail}, limit {} bps", cfg.max_roundtrip_loss_bps),
        ));
    }

    // --- depth probe -----------------------------------------------------
    // Ask for a quote at the size we claim to need. A pool that cannot absorb
    // it without extreme impact is a pool you cannot exit at size, no matter
    // what its headline TVL says.
    let depth_lamports = (cfg.min_liquidity_sol * LAMPORTS_PER_SOL) as u64;
    match jup.quote(wsol, mint, depth_lamports, 500, Priority::Screening).await {
        Ok(Some(q)) if q.price_impact_pct <= MAX_DEPTH_PROBE_IMPACT => {
            out.push(CheckResult::pass(
                "depth",
                format!(
                    "absorbs {:.2} SOL at {:.1}% impact",
                    cfg.min_liquidity_sol,
                    q.price_impact_pct * 100.0
                ),
            ));
        }
        Ok(Some(q)) => {
            out.push(CheckResult::fail(
                "depth",
                format!(
                    "{:.2} SOL moves price {:.1}% - pool too thin",
                    cfg.min_liquidity_sol,
                    q.price_impact_pct * 100.0
                ),
            ));
        }
        Ok(None) => {
            out.push(CheckResult::fail(
                "depth",
                format!("cannot route {:.2} SOL - pool too thin", cfg.min_liquidity_sol),
            ));
        }
        Err(e) => {
            out.push(CheckResult::unavailable(
                "depth",
                format!("depth probe failed: {e}"),
            ));
        }
    }

    (out, Some(loss_bps), Some(entry_price))
}
