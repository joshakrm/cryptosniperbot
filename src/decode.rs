use std::collections::HashMap;

use serde_json::Value;

use crate::types::{Pubkey, Venue};

/// True when `marker` is logged while `program_id` is the innermost executing
/// program.
///
/// This scoping is essential, not a refinement. `logsSubscribe` with a
/// `mentions` filter delivers the WHOLE log stream of any transaction that
/// touches the program, including lines emitted by every other program in the
/// transaction. Matching markers anywhere in that stream means an arbitrage bot
/// routing a swap through Raydium reads as a brand-new pool - observed live,
/// and it costs an RPC round trip per false positive.
///
/// Solana log streams are a flat rendering of a call tree:
///
///   Program <id> invoke [1]
///   Program log: Instruction: Create
///   Program <id> success
///
/// so tracking invoke/success gives us the program that actually spoke.
fn marker_under_program(logs: &[String], program_id: &str, marker: &str) -> bool {
    let mut stack: Vec<&str> = Vec::new();

    for line in logs {
        // Output lines belong to whichever program is currently innermost.
        if line.starts_with("Program log:") || line.starts_with("Program data:") {
            if line.contains(marker) && stack.last() == Some(&program_id) {
                return true;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("Program ") {
            let mut parts = rest.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(id), Some("invoke")) => stack.push(id),
                // "failed" carries a trailing reason, hence the prefix test.
                (Some(_), Some(verb)) if verb == "success" || verb.starts_with("failed") => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
    false
}

/// Does this log stream look like a pool/token creation on the given venue?
///
/// The markers are strings the programs emit via their own `msg!` calls. They
/// are the cheapest possible filter and they WILL drift when programs are
/// upgraded. If detections dry up, this function is the first place to look.
pub fn is_launch_log(venue: Venue, program_id: &str, logs: &[String]) -> bool {
    let hit = |marker: &str| marker_under_program(logs, program_id, marker);
    match venue {
        // "Instruction: Create" is a prefix of "Instruction: CreateTokenAccount",
        // so the negative guard is what stops that false positive.
        Venue::PumpFun => hit("Instruction: Create") && !hit("Instruction: CreateTokenAccount"),
        Venue::PumpSwap => hit("Instruction: CreatePool"),
        Venue::RaydiumAmmV4 => hit("initialize2"),
        Venue::RaydiumCpmm => hit("Instruction: Initialize"),
        Venue::Unknown => false,
    }
}

/// Pull the launched mint out of a confirmed transaction.
///
/// Venue-agnostic on purpose: rather than decoding each program account layout
/// (which breaks on every upgrade), we look at the token balances the
/// transaction touched. This costs one extra RPC round trip but survives
/// program upgrades and works identically across pump.fun, Raydium and PumpSwap.
///
/// The wrinkle is that a pool creation mints LP tokens to the creator alongside
/// the base token, so "exactly one non-quote mint" is too strict and silently
/// drops every PumpSwap and Raydium launch. Observed live: 4 detected
/// CreatePool calls, 0 candidates.
///
/// The discriminator is ownership. At creation the LP mint exists ONLY in the
/// creator's hands, while the base token is also sitting in the pool vault. So
/// among several non-quote mints, the one with a holder other than the fee
/// payer is the launch. A genuine multi-hop or arbitrage transaction has
/// several such mints and is still rejected, which is what we want.
pub fn extract_mint(tx: &Value, quote_mints: &[&str]) -> Option<Pubkey> {
    let meta = tx.get("meta")?;
    let fee_payer = extract_creator(tx);

    let mut order: Vec<String> = Vec::new();
    let mut vaulted: HashMap<String, bool> = HashMap::new();

    for key in ["postTokenBalances", "preTokenBalances"] {
        let arr = match meta.get(key).and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        for entry in arr {
            let mint = match entry.get("mint").and_then(|v| v.as_str()) {
                Some(m) if !quote_mints.contains(&m) => m,
                _ => continue,
            };
            if !order.iter().any(|c| c == mint) {
                order.push(mint.to_string());
            }
            // Held by anyone other than the fee payer?
            let third_party = match (
                entry.get("owner").and_then(|v| v.as_str()),
                fee_payer.as_deref(),
            ) {
                (Some(owner), Some(payer)) => owner != payer,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let flag = vaulted.entry(mint.to_string()).or_insert(false);
            *flag = *flag || third_party;
        }
    }

    if order.len() == 1 {
        return order.into_iter().next();
    }

    let mut held_elsewhere: Vec<String> = order
        .into_iter()
        .filter(|m| vaulted.get(m).copied().unwrap_or(false))
        .collect();

    // Exactly one mint reached a vault: that is the launch. Zero or several
    // means we cannot tell, and guessing is how you buy an arbitrage leg.
    if held_elsewhere.len() == 1 {
        Some(held_elsewhere.remove(0))
    } else {
        None
    }
}

/// The LP mint a pool creation minted, when there is one.
///
/// Same ownership discriminator as `extract_mint`, read the other way round:
/// the base token reaches a pool vault, while LP tokens exist only in the
/// creator's hands at creation.
///
/// Returning None is meaningful, not a failure. A bonding-curve launch
/// (pump.fun before migration) has no LP mint at all, and that is the safest
/// possible answer - there is no liquidity position for anyone to withdraw.
pub fn extract_lp_mint(tx: &Value, base_mint: &str, quote_mints: &[&str]) -> Option<Pubkey> {
    let meta = tx.get("meta")?;
    let payer = extract_creator(tx)?;

    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for key in ["postTokenBalances", "preTokenBalances"] {
        let arr = match meta.get(key).and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        for entry in arr {
            let mint = match entry.get("mint").and_then(|v| v.as_str()) {
                Some(m) if m != base_mint && !quote_mints.contains(&m) => m,
                _ => continue,
            };
            if let Some(owner) = entry.get("owner").and_then(|v| v.as_str()) {
                owners
                    .entry(mint.to_string())
                    .or_default()
                    .push(owner.to_string());
            }
        }
    }

    // Held by the creator and nobody else.
    owners
        .into_iter()
        .find(|(_, os)| !os.is_empty() && os.iter().all(|o| *o == payer))
        .map(|(m, _)| m)
}

/// The pool's token account for `mint`, when this transaction created one.
///
/// getTokenLargestAccounts returns token ACCOUNT addresses, while balance
/// entries carry the OWNER. They are different keys, so the concentration check
/// cannot exclude the pool by owner. Balance entries also carry accountIndex,
/// which indexes the transaction's account keys - and that gives the address.
///
/// Without this, concentration has to exclude the largest holder positionally,
/// which drops a whale whenever a whale outranks the vault. That is exactly the
/// shape of the rug the check exists to catch.
pub fn extract_vault_account(tx: &Value, mint: &str) -> Option<Pubkey> {
    let meta = tx.get("meta")?;
    let fee_payer = extract_creator(tx);
    let keys = tx
        .get("transaction")?
        .get("message")?
        .get("accountKeys")?
        .as_array()?;

    for key in ["postTokenBalances", "preTokenBalances"] {
        let arr = match meta.get(key).and_then(|v| v.as_array()) {
            Some(a) => a,
            None => continue,
        };
        for entry in arr {
            if entry.get("mint").and_then(|v| v.as_str()) != Some(mint) {
                continue;
            }
            // The creator's own account is not the pool.
            let third_party = match (
                entry.get("owner").and_then(|v| v.as_str()),
                fee_payer.as_deref(),
            ) {
                (Some(owner), Some(payer)) => owner != payer,
                _ => false,
            };
            if !third_party {
                continue;
            }
            let idx = match entry.get("accountIndex").and_then(|v| v.as_u64()) {
                Some(i) => i as usize,
                None => continue,
            };
            if let Some(k) = keys.get(idx) {
                if let Some(addr) = k
                    .get("pubkey")
                    .and_then(|v| v.as_str())
                    .or_else(|| k.as_str())
                {
                    return Some(addr.to_string());
                }
            }
        }
    }
    None
}

/// Two numbers the launch transaction already contains, for free.
///
/// The transaction is fetched anyway to find the mint, so reading more out of it
/// costs nothing - and cost is the whole problem. The holder checks that made
/// the aggregator budget viable need an index that lags launches by ~13s, and
/// the checks that survive without it reject only 3% of candidates. Anything
/// that filters at t=0 has to come out of data already in hand.
///
/// Returns (creator share of supply as a percentage, SOL placed in the pool).
/// Recorded rather than enforced: thresholds should come from a measured
/// distribution, not from a guess.
pub fn extract_launch_metrics(
    tx: &Value,
    mint: &str,
) -> (Option<f64>, Option<f64>) {
    let meta = match tx.get("meta") {
        Some(m) => m,
        None => return (None, None),
    };
    let payer = extract_creator(tx);

    // --- creator share of the launched token -----------------------------
    let mut total = 0.0f64;
    let mut creator = 0.0f64;
    if let Some(arr) = meta.get("postTokenBalances").and_then(|v| v.as_array()) {
        for e in arr {
            if e.get("mint").and_then(|v| v.as_str()) != Some(mint) {
                continue;
            }
            let amt = e
                .get("uiTokenAmount")
                .and_then(|u| u.get("uiAmount"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            total += amt;
            if let (Some(owner), Some(p)) = (e.get("owner").and_then(|v| v.as_str()), payer.as_deref())
            {
                if owner == p {
                    creator += amt;
                }
            }
        }
    }
    let share = if total > 0.0 {
        Some(100.0 * creator / total)
    } else {
        None
    };

    // --- SOL that landed somewhere other than the fee payer --------------
    // The largest positive lamport delta is the pool or bonding curve being
    // funded. Index 0 is the fee payer and is skipped.
    let pre = meta.get("preBalances").and_then(|v| v.as_array());
    let post = meta.get("postBalances").and_then(|v| v.as_array());
    let pool_sol = match (pre, post) {
        (Some(pre), Some(post)) => {
            let mut best = 0.0f64;
            for i in 1..pre.len().min(post.len()) {
                let a = pre[i].as_f64().unwrap_or(0.0);
                let b = post[i].as_f64().unwrap_or(0.0);
                let delta = (b - a) / crate::types::LAMPORTS_PER_SOL;
                if delta > best {
                    best = delta;
                }
            }
            Some(best)
        }
        _ => None,
    };

    (share, pool_sol)
}

/// Fee payer of the transaction: the cheapest available proxy for "the dev".
pub fn extract_creator(tx: &Value) -> Option<Pubkey> {
    tx.get("transaction")?
        .get("message")?
        .get("accountKeys")?
        .as_array()?
        .first()
        .and_then(|k| {
            k.get("pubkey")
                .and_then(|v| v.as_str())
                .or_else(|| k.as_str())
        })
        .map(|s| s.to_string())
}

/// Decimals for the launched mint, read from balance entries we already have.
pub fn extract_decimals(tx: &Value, mint: &str) -> Option<u8> {
    let meta = tx.get("meta")?;
    for key in ["postTokenBalances", "preTokenBalances"] {
        if let Some(arr) = meta.get(key).and_then(|v| v.as_array()) {
            for entry in arr {
                if entry.get("mint").and_then(|v| v.as_str()) == Some(mint) {
                    if let Some(d) = entry
                        .get("uiTokenAmount")
                        .and_then(|u| u.get("decimals"))
                        .and_then(|v| v.as_u64())
                    {
                        return Some(d as u8);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const NEW: &str = "NEWMINT1111111111111111111111111111111111111";

    fn tx(mints: &[&str]) -> serde_json::Value {
        let balances: Vec<_> = mints
            .iter()
            .enumerate()
            .map(|(i, m)| {
                json!({
                    "accountIndex": i,
                    "mint": m,
                    "uiTokenAmount": { "amount": "1000", "decimals": 6, "uiAmount": 0.001 }
                })
            })
            .collect();
        json!({
            "meta": { "postTokenBalances": balances, "preTokenBalances": [] },
            "transaction": {
                "message": { "accountKeys": [{ "pubkey": "DEV111" }, { "pubkey": "OTHER" }] }
            }
        })
    }

    #[test]
    fn extracts_the_single_non_quote_mint() {
        assert_eq!(extract_mint(&tx(&[WSOL, NEW]), &[WSOL, USDC]), Some(NEW.to_string()));
    }

    #[test]
    fn rejects_a_transaction_touching_two_unknown_mints() {
        // A multi-hop or aggregator transaction, not a launch we can trust.
        let t = tx(&[WSOL, NEW, "OTHERMINT2222222222222222222222222222222222"]);
        assert_eq!(extract_mint(&t, &[WSOL, USDC]), None);
    }

    #[test]
    fn rejects_a_transaction_with_only_quote_mints() {
        assert_eq!(extract_mint(&tx(&[WSOL, USDC]), &[WSOL, USDC]), None);
    }

    /// Balances with explicit owners. The fee payer in `tx` is DEV111.
    fn tx_owned(entries: &[(&str, &str)]) -> serde_json::Value {
        let balances: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(i, (mint, owner))| {
                json!({
                    "accountIndex": i,
                    "mint": mint,
                    "owner": owner,
                    "uiTokenAmount": { "amount": "1000", "decimals": 6, "uiAmount": 0.001 }
                })
            })
            .collect();
        json!({
            "meta": { "postTokenBalances": balances, "preTokenBalances": [] },
            "transaction": {
                "message": { "accountKeys": [{ "pubkey": "DEV111" }, { "pubkey": "OTHER" }] }
            }
        })
    }

    // REGRESSION: observed live. A PumpSwap CreatePool mints LP tokens to the
    // creator alongside the base token, so requiring exactly one non-quote mint
    // dropped every PumpSwap launch: 4 detected, 0 candidates.
    #[test]
    fn picks_the_base_mint_when_an_lp_mint_was_also_created() {
        const LP: &str = "LPMINT11111111111111111111111111111111111111";
        let t = tx_owned(&[
            (WSOL, "VAULT"),
            (LP, "DEV111"),  // LP exists only in the creator's hands
            (NEW, "VAULT"),  // base token also reached the pool vault
            (NEW, "DEV111"),
        ]);
        assert_eq!(extract_mint(&t, &[WSOL, USDC]), Some(NEW.to_string()));
    }

    const LPM: &str = "LPMINT11111111111111111111111111111111111111";

    fn tx_with_sol(entries: &[(&str, &str, f64)], pre: &[u64], post: &[u64]) -> serde_json::Value {
        let balances: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(i, (mint, owner, amt))| {
                json!({
                    "accountIndex": i,
                    "mint": mint,
                    "owner": owner,
                    "uiTokenAmount": { "amount": "0", "decimals": 6, "uiAmount": amt }
                })
            })
            .collect();
        json!({
            "meta": {
                "postTokenBalances": balances,
                "preTokenBalances": [],
                "preBalances": pre,
                "postBalances": post
            },
            "transaction": {
                "message": { "accountKeys": [{ "pubkey": "DEV111" }, { "pubkey": "OTHER" }] }
            }
        })
    }

    #[test]
    fn reads_the_creator_share_of_supply() {
        // Curve holds 600, creator holds 400 -> 40%.
        let t = tx_with_sol(
            &[(NEW, "CURVE", 600.0), (NEW, "DEV111", 400.0)],
            &[0, 0],
            &[0, 0],
        );
        let (share, _) = extract_launch_metrics(&t, NEW);
        assert!((share.unwrap() - 40.0).abs() < 1e-9, "{share:?}");
    }

    #[test]
    fn reads_the_sol_placed_in_the_pool() {
        // Index 0 is the fee payer and must be ignored even if it gained.
        let t = tx_with_sol(
            &[(NEW, "CURVE", 1.0)],
            &[0, 0, 0],
            &[9_000_000_000, 2_500_000_000, 100],
        );
        let (_, sol) = extract_launch_metrics(&t, NEW);
        assert!((sol.unwrap() - 2.5).abs() < 1e-9, "{sol:?}");
    }

    #[test]
    fn launch_metrics_survive_a_transaction_with_nothing_in_it() {
        let (share, sol) = extract_launch_metrics(&json!({}), NEW);
        assert_eq!(share, None);
        assert_eq!(sol, None);
    }

    #[test]
    fn finds_the_lp_mint_a_pool_creation_minted() {
        // Real PumpSwap shape: quote and base reach the vault, LP does not.
        let t = tx_owned(&[
            (WSOL, "VAULT"),
            (LPM, "DEV111"),
            (NEW, "VAULT"),
            (NEW, "DEV111"),
        ]);
        assert_eq!(extract_lp_mint(&t, NEW, &[WSOL, USDC]), Some(LPM.to_string()));
    }

    #[test]
    fn a_bonding_curve_launch_has_no_lp_mint() {
        // pump.fun before migration: no third mint exists, so there is no
        // liquidity position for anyone to withdraw. None is the safe answer.
        let t = tx_owned(&[(WSOL, "CURVE"), (NEW, "CURVE"), (NEW, "DEV111")]);
        assert_eq!(extract_lp_mint(&t, NEW, &[WSOL, USDC]), None);
    }

    #[test]
    fn the_base_mint_is_never_mistaken_for_the_lp_mint() {
        // The creator holds base tokens too; that must not make it look like LP.
        let t = tx_owned(&[(WSOL, "VAULT"), (NEW, "DEV111")]);
        assert_eq!(extract_lp_mint(&t, NEW, &[WSOL, USDC]), None);
    }

    #[test]
    fn rejects_when_several_mints_reached_a_vault() {
        // The arbitrage shape: two real tokens, both held by third parties.
        // Guessing between them is how you buy someone else's swap leg.
        const OTHER: &str = "OTHERMINT2222222222222222222222222222222222";
        let t = tx_owned(&[(NEW, "PARTY_A"), (OTHER, "PARTY_B")]);
        assert_eq!(extract_mint(&t, &[WSOL, USDC]), None);
    }

    #[test]
    fn rejects_when_only_the_creator_holds_anything() {
        // No mint reached a vault, so nothing is identifiable as the launch.
        const LP: &str = "LPMINT11111111111111111111111111111111111111";
        let t = tx_owned(&[(LP, "DEV111"), (NEW, "DEV111")]);
        assert_eq!(extract_mint(&t, &[WSOL, USDC]), None);
    }

    #[test]
    fn reads_decimals_for_the_launched_mint() {
        assert_eq!(extract_decimals(&tx(&[WSOL, NEW]), NEW), Some(6));
        assert_eq!(extract_decimals(&tx(&[WSOL, NEW]), "ABSENT"), None);
    }

    #[test]
    fn creator_is_the_fee_payer() {
        assert_eq!(extract_creator(&tx(&[WSOL, NEW])), Some("DEV111".to_string()));
    }

    #[test]
    fn malformed_input_yields_none_rather_than_panicking() {
        assert_eq!(extract_mint(&json!({}), &[WSOL]), None);
        assert_eq!(extract_creator(&json!({})), None);
        assert_eq!(extract_decimals(&json!({}), NEW), None);
    }

    const PUMP: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
    const RAY: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

    fn stream(program: &str, inner: &[&str]) -> Vec<String> {
        let mut v = vec![format!("Program {program} invoke [1]")];
        v.extend(inner.iter().map(|s| s.to_string()));
        v.push(format!("Program {program} success"));
        v
    }

    #[test]
    fn launch_markers_are_venue_specific() {
        let ray = stream(RAY, &["Program log: initialize2: InitializeInstruction2"]);
        assert!(is_launch_log(Venue::RaydiumAmmV4, RAY, &ray));
        assert!(!is_launch_log(Venue::Unknown, RAY, &ray));

        let pump = stream(PUMP, &["Program log: Instruction: Create"]);
        assert!(is_launch_log(Venue::PumpFun, PUMP, &pump));
    }

    #[test]
    fn token_account_creation_is_not_a_launch() {
        let noise = stream(PUMP, &["Program log: Instruction: CreateTokenAccount"]);
        assert!(!is_launch_log(Venue::PumpFun, PUMP, &noise));
    }

    // REGRESSION: observed live. An arbitrage bot routing through Raydium emits
    // its own "Instruction: Initialize" inside a nested program, and the whole
    // stream arrives on the Raydium subscription. Unscoped matching read that as
    // a new pool.
    #[test]
    fn a_marker_from_a_nested_program_is_not_our_venue() {
        let logs = vec![
            format!("Program {RAY} invoke [1]"),
            format!("Program {PUMP} invoke [2]"),
            "Program log: Instruction: Initialize".to_string(),
            format!("Program {PUMP} success"),
            format!("Program {RAY} success"),
        ];
        assert!(
            !is_launch_log(Venue::RaydiumCpmm, RAY, &logs),
            "a nested program's log must not be attributed to the outer program"
        );
    }

    #[test]
    fn a_marker_after_the_program_returns_is_not_ours() {
        let logs = vec![
            format!("Program {RAY} invoke [1]"),
            format!("Program {RAY} success"),
            "Program log: initialize2".to_string(),
        ];
        assert!(!is_launch_log(Venue::RaydiumAmmV4, RAY, &logs));
    }

    #[test]
    fn a_failed_inner_program_still_pops_the_stack() {
        let logs = vec![
            format!("Program {RAY} invoke [1]"),
            format!("Program {PUMP} invoke [2]"),
            format!("Program {PUMP} failed: custom program error: 0x1"),
            "Program log: initialize2".to_string(),
            format!("Program {RAY} success"),
        ];
        assert!(is_launch_log(Venue::RaydiumAmmV4, RAY, &logs));
    }
}
