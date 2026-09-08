//! Detecting what a followed wallet actually did, from the transaction itself.
//!
//! The sniper's question was "has a new pool appeared". This one is "has a
//! wallet I follow bought or sold something", which is a different signal on the
//! same plumbing: logsSubscribe takes any pubkey in its `mentions` filter, so a
//! wallet subscribes exactly the way a program does. Verified against the free
//! endpoint before this module existed - a wallet filter is accepted and an idle
//! wallet simply reports nothing, which is not the same as being refused.
//!
//! WHY BALANCE DELTAS AND NOT INSTRUCTION PARSING. A trader on FOMO or anywhere
//! else routes through whatever venue is cheapest at that moment - pump.fun,
//! PumpSwap, Raydium, Meteora, a Jupiter route through three of them. Decoding
//! each venue's instruction layout means a decoder per venue, silently missing
//! every trade through one we have not implemented, and breaking whenever a
//! program is upgraded.
//!
//! The token balances before and after are venue-agnostic and already in the
//! transaction we fetch anyway. If the wallet's balance of some mint went up and
//! its SOL went down, it bought; the reverse is a sell. That is true regardless
//! of how the trade was routed, and it cannot silently miss a venue.
//!
//! WHAT IT CANNOT SEE: a trade that never touches the followed wallet's own
//! token accounts - routed through a program-owned intermediate, or done by a
//! different wallet the same person controls. Following one address means
//! following one address.

use serde_json::Value;

/// Which way the followed wallet went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Buy,
    Sell,
}

/// A trade observed on a followed wallet.
#[derive(Debug, Clone)]
pub struct Swap {
    pub wallet: String,
    pub mint: String,
    pub direction: Direction,
    /// Whole tokens that moved, always positive.
    pub tokens: f64,
    /// SOL that moved, always positive. Includes the network fee on the native
    /// side, which is immaterial next to a trade and not worth pretending to
    /// separate.
    pub sol: f64,
    pub decimals: u8,
}

impl Swap {
    /// SOL per whole token. None when either side is zero - an airdrop or a
    /// transfer is not a trade and must not be priced as one.
    pub fn price(&self) -> Option<f64> {
        if self.tokens > 0.0 && self.sol > 0.0 {
            Some(self.sol / self.tokens)
        } else {
            None
        }
    }
}

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Balance of `mint` held by `owner`, before and after, in whole tokens.
fn token_delta(meta: &Value, owner: &str, key: &str) -> Vec<(String, f64, u8)> {
    let mut out = Vec::new();
    let arr = match meta.get(key).and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return out,
    };
    for e in arr {
        if e.get("owner").and_then(|v| v.as_str()) != Some(owner) {
            continue;
        }
        let mint = match e.get("mint").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => continue,
        };
        let ui = e.get("uiTokenAmount");
        let amount = ui
            .and_then(|u| u.get("uiAmountString"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let decimals = ui
            .and_then(|u| u.get("decimals"))
            .and_then(|v| v.as_u64())
            .unwrap_or(9) as u8;
        out.push((mint, amount, decimals));
    }
    out
}

/// What the wallet's SOL balance did, in SOL. Negative means it was spent.
///
/// Counts both the native balance and any wrapped-SOL token account, because a
/// router may settle either way and treating a WSOL-settled trade as having no
/// SOL side would misprice it entirely.
fn sol_delta(tx: &Value, meta: &Value, wallet: &str, wsol: &str) -> f64 {
    let mut delta = 0.0;

    if let (Some(keys), Some(pre), Some(post)) = (
        tx.get("transaction")
            .and_then(|t| t.get("message"))
            .and_then(|m| m.get("accountKeys"))
            .and_then(|v| v.as_array()),
        meta.get("preBalances").and_then(|v| v.as_array()),
        meta.get("postBalances").and_then(|v| v.as_array()),
    ) {
        for (i, k) in keys.iter().enumerate() {
            let pubkey = k
                .get("pubkey")
                .and_then(|v| v.as_str())
                .or_else(|| k.as_str());
            if pubkey != Some(wallet) {
                continue;
            }
            let a = pre.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let b = post.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
            delta += (b - a) / LAMPORTS_PER_SOL;
        }
    }

    let pre_w: f64 = token_delta(meta, wallet, "preTokenBalances")
        .into_iter()
        .filter(|(m, _, _)| m == wsol)
        .map(|(_, a, _)| a)
        .sum();
    let post_w: f64 = token_delta(meta, wallet, "postTokenBalances")
        .into_iter()
        .filter(|(m, _, _)| m == wsol)
        .map(|(_, a, _)| a)
        .sum();
    delta += post_w - pre_w;

    delta
}

/// The trade a followed wallet made in this transaction, if it made one.
///
/// Returns None for anything that is not a two-sided swap: failed transactions,
/// transfers, airdrops, approvals. Being strict here matters more than catching
/// every edge - a false positive makes the bot buy something nobody bought.
pub fn detect_swap(tx: &Value, wallet: &str, quote_mints: &[&str]) -> Option<Swap> {
    let meta = tx.get("meta")?;

    // A reverted transaction moved nothing. Copying one would be copying an
    // intention rather than a trade.
    if !meta.get("err").map(|e| e.is_null()).unwrap_or(true) {
        return None;
    }

    let wsol = quote_mints.first().copied().unwrap_or("");

    let mut pre: Vec<(String, f64, u8)> = token_delta(meta, wallet, "preTokenBalances");
    let post: Vec<(String, f64, u8)> = token_delta(meta, wallet, "postTokenBalances");

    // Largest non-quote position change, so a route that touches several mints
    // is attributed to the one the trader actually took a position in.
    let mut best: Option<(String, f64, u8)> = None;
    for (mint, after, decimals) in &post {
        if quote_mints.contains(&mint.as_str()) {
            continue;
        }
        let before = pre
            .iter()
            .find(|(m, _, _)| m == mint)
            .map(|(_, a, _)| *a)
            .unwrap_or(0.0);
        let change = after - before;
        if change.abs() <= 0.0 {
            continue;
        }
        if best
            .as_ref()
            .map(|(_, c, _)| change.abs() > c.abs())
            .unwrap_or(true)
        {
            best = Some((mint.clone(), change, *decimals));
        }
    }

    // A sell that empties an account leaves no post balance for that mint, so
    // the disappearance has to be looked for on the pre side too.
    if best.is_none() {
        pre.retain(|(m, a, _)| !quote_mints.contains(&m.as_str()) && *a > 0.0);
        for (mint, before, decimals) in &pre {
            if post.iter().any(|(m, _, _)| m == mint) {
                continue;
            }
            if best
                .as_ref()
                .map(|(_, c, _)| before.abs() > c.abs())
                .unwrap_or(true)
            {
                best = Some((mint.clone(), -before, *decimals));
            }
        }
    }

    let (mint, token_change, decimals) = best?;
    let sol_change = sol_delta(tx, meta, wallet, wsol);

    // Both sides must move, in opposite directions. A token arriving with no
    // SOL leaving is an airdrop; SOL leaving with no token arriving is a
    // transfer or a fee. Neither is a trade to copy.
    let direction = if token_change > 0.0 && sol_change < 0.0 {
        Direction::Buy
    } else if token_change < 0.0 && sol_change > 0.0 {
        Direction::Sell
    } else {
        return None;
    };

    Some(Swap {
        wallet: wallet.to_string(),
        mint,
        direction,
        tokens: token_change.abs(),
        sol: sol_change.abs(),
        decimals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const W: &str = "TraderWa11etAddress11111111111111111111111";
    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const TOK: &str = "SomeMemeCoinMint1111111111111111111111111111";

    fn quotes() -> Vec<&'static str> {
        vec![WSOL, USDC]
    }

    /// A transaction where the wallet's native SOL and one token both move.
    fn tx(pre_tok: f64, post_tok: f64, pre_sol: f64, post_sol: f64) -> Value {
        let mut pre = vec![];
        let mut post = vec![];
        if pre_tok > 0.0 {
            pre.push(json!({"owner": W, "mint": TOK,
                "uiTokenAmount": {"uiAmountString": pre_tok.to_string(), "decimals": 6}}));
        }
        if post_tok > 0.0 {
            post.push(json!({"owner": W, "mint": TOK,
                "uiTokenAmount": {"uiAmountString": post_tok.to_string(), "decimals": 6}}));
        }
        json!({
            "meta": {
                "err": null,
                "preBalances": [(pre_sol * 1e9) as u64],
                "postBalances": [(post_sol * 1e9) as u64],
                "preTokenBalances": pre,
                "postTokenBalances": post,
            },
            "transaction": {"message": {"accountKeys": [{"pubkey": W, "signer": true}]}}
        })
    }

    #[test]
    fn a_buy_is_tokens_in_and_sol_out() {
        let s = detect_swap(&tx(0.0, 1_000.0, 5.0, 4.5), W, &quotes()).expect("a buy");
        assert_eq!(s.direction, Direction::Buy);
        assert_eq!(s.mint, TOK);
        assert!((s.tokens - 1_000.0).abs() < 1e-9);
        assert!((s.sol - 0.5).abs() < 1e-9, "sol was {}", s.sol);
        assert!((s.price().unwrap() - 0.0005).abs() < 1e-9);
    }

    #[test]
    fn a_sell_is_tokens_out_and_sol_in() {
        let s = detect_swap(&tx(1_000.0, 0.0, 4.5, 5.0), W, &quotes()).expect("a sell");
        assert_eq!(s.direction, Direction::Sell);
        assert!((s.tokens - 1_000.0).abs() < 1e-9);
        assert!((s.sol - 0.5).abs() < 1e-9);
    }

    // Selling the whole position closes the token account, so the mint vanishes
    // from postTokenBalances entirely. Reading only the post side would miss
    // every full exit - which is exactly the event a mirror must not miss.
    #[test]
    fn a_full_exit_that_closes_the_account_is_still_seen() {
        let mut t = tx(1_000.0, 0.0, 4.5, 5.0);
        t["meta"]["postTokenBalances"] = json!([]);
        let s = detect_swap(&t, W, &quotes()).expect("a full exit");
        assert_eq!(s.direction, Direction::Sell);
        assert!((s.tokens - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn a_failed_transaction_is_not_a_trade() {
        let mut t = tx(0.0, 1_000.0, 5.0, 4.5);
        t["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
        assert!(detect_swap(&t, W, &quotes()).is_none());
    }

    // Tokens arriving for free is an airdrop. Copying it would have the bot buy
    // something the trader never bought - and airdrops to known-good wallets are
    // a cheap way to make a follower buy your token.
    #[test]
    fn an_airdrop_is_not_a_buy() {
        let t = tx(0.0, 1_000.0, 5.0, 5.0);
        assert!(detect_swap(&t, W, &quotes()).is_none());
    }

    #[test]
    fn sol_leaving_with_no_token_arriving_is_not_a_trade() {
        let t = tx(0.0, 0.0, 5.0, 4.0);
        assert!(detect_swap(&t, W, &quotes()).is_none());
    }

    #[test]
    fn another_wallets_activity_is_ignored() {
        let mut t = tx(0.0, 1_000.0, 5.0, 4.5);
        t["meta"]["postTokenBalances"][0]["owner"] = json!("SomeoneE1se1111111111111111111");
        assert!(detect_swap(&t, W, &quotes()).is_none());
    }

    // Quote mints are the money side, never the position. A route that leaves
    // the wallet holding USDC has not taken a position in USDC.
    #[test]
    fn quote_mints_are_never_the_traded_token() {
        let t = json!({
            "meta": {
                "err": null,
                "preBalances": [5_000_000_000u64],
                "postBalances": [4_500_000_000u64],
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"owner": W, "mint": USDC,
                     "uiTokenAmount": {"uiAmountString": "50.0", "decimals": 6}}
                ],
            },
            "transaction": {"message": {"accountKeys": [{"pubkey": W, "signer": true}]}}
        });
        assert!(detect_swap(&t, W, &quotes()).is_none());
    }

    // Routers often settle in wrapped SOL rather than moving the native
    // balance. Ignoring the WSOL leg would read a real buy as an airdrop.
    #[test]
    fn a_trade_settled_in_wrapped_sol_is_still_a_trade() {
        let t = json!({
            "meta": {
                "err": null,
                "preBalances": [5_000_000_000u64],
                "postBalances": [5_000_000_000u64],
                "preTokenBalances": [
                    {"owner": W, "mint": WSOL,
                     "uiTokenAmount": {"uiAmountString": "2.0", "decimals": 9}}
                ],
                "postTokenBalances": [
                    {"owner": W, "mint": WSOL,
                     "uiTokenAmount": {"uiAmountString": "1.5", "decimals": 9}},
                    {"owner": W, "mint": TOK,
                     "uiTokenAmount": {"uiAmountString": "1000.0", "decimals": 6}}
                ],
            },
            "transaction": {"message": {"accountKeys": [{"pubkey": W, "signer": true}]}}
        });
        let s = detect_swap(&t, W, &quotes()).expect("wsol-settled buy");
        assert_eq!(s.direction, Direction::Buy);
        assert!((s.sol - 0.5).abs() < 1e-9, "sol was {}", s.sol);
    }

    // A multi-hop route can touch several mints; the position is the largest
    // move, not whichever happens to be listed first.
    #[test]
    fn the_largest_position_change_wins_a_multi_hop_route() {
        let t = json!({
            "meta": {
                "err": null,
                "preBalances": [5_000_000_000u64],
                "postBalances": [4_000_000_000u64],
                "preTokenBalances": [],
                "postTokenBalances": [
                    {"owner": W, "mint": "DustMint111111111111111111111111",
                     "uiTokenAmount": {"uiAmountString": "1.0", "decimals": 6}},
                    {"owner": W, "mint": TOK,
                     "uiTokenAmount": {"uiAmountString": "9999.0", "decimals": 6}}
                ],
            },
            "transaction": {"message": {"accountKeys": [{"pubkey": W, "signer": true}]}}
        });
        let s = detect_swap(&t, W, &quotes()).expect("a buy");
        assert_eq!(s.mint, TOK);
    }

    #[test]
    fn a_swap_with_no_token_side_has_no_price() {
        let s = Swap {
            wallet: W.into(),
            mint: TOK.into(),
            direction: Direction::Buy,
            tokens: 0.0,
            sol: 1.0,
            decimals: 6,
        };
        assert!(s.price().is_none());
    }
}
