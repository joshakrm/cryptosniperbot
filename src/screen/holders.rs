use super::MintFacts;
use crate::config::ScreenConfig;
use crate::rpc::SolanaRpc;
use crate::types::{CheckResult, Pubkey, Venue};

/// Holder-distribution checks.
///
/// A caveat worth understanding before you tune the thresholds: for a brand-new
/// pump.fun launch, the bonding curve account legitimately holds nearly the
/// entire supply, so concentration numbers are meaningless at t=0. We drop the
/// single largest holder as the presumed pool/curve, and downgrade the check to
/// advisory on PumpFun rather than pretending it means something there. On a
/// Raydium pool or a graduated token, it means a great deal.
pub async fn check(
    rpc: &SolanaRpc,
    cfg: &ScreenConfig,
    mint: &Pubkey,
    facts: &MintFacts,
    venue: Venue,
    // The pool's token account, when the launch transaction revealed it.
    // None means measure every holder rather than guess which one is the pool.
    vault: Option<&str>,
) -> Vec<CheckResult> {
    if !cfg.require_holders {
        // Deliberately silent rather than an advisory note: an operator who
        // turned this off knows it is off, and a per-candidate reminder would
        // just be noise in the journal.
        return Vec::new();
    }

    let mut out = Vec::new();

    let resp = match rpc.get_token_largest_accounts(mint).await {
        Ok(v) => v,
        Err(e) => {
            out.push(CheckResult::unavailable(
                "holders",
                format!("could not read largest accounts: {e}"),
            ));
            return out;
        }
    };

    let accounts = match resp.get("value").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => {
            out.push(CheckResult::unavailable(
                "holders",
                "largest accounts response malformed",
            ));
            return out;
        }
    };

    if accounts.is_empty() {
        out.push(CheckResult::fail("holders", "no token accounts exist yet"));
        return out;
    }

    // Addresses are kept, not just amounts: excluding the pool by position is
    // only safe if the pool really is the largest account, and often it is not.
    let mut holdings: Vec<(String, u128)> = accounts
        .iter()
        .filter_map(|a| {
            let amount = a
                .get("amount")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u128>().ok())?;
            if amount == 0 {
                return None;
            }
            let address = a
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some((address, amount))
        })
        .collect();
    holdings.sort_unstable_by_key(|h| std::cmp::Reverse(h.1));
    let amounts: Vec<u128> = holdings.iter().map(|(_, amt)| *amt).collect();

    // getTokenLargestAccounts caps at 20 entries, so this is a floor on the
    // true holder count, not the real number. Treated as a floor deliberately.
    let holder_floor = amounts.len() as u64;
    if holder_floor < cfg.min_holders {
        out.push(CheckResult::fail(
            "holder_count",
            format!("only {holder_floor} funded accounts, need {}", cfg.min_holders),
        ));
    } else {
        out.push(CheckResult::pass(
            "holder_count",
            format!("at least {holder_floor} funded accounts"),
        ));
    }

    if facts.supply_raw == 0 {
        out.push(CheckResult::fail("concentration", "supply is zero, cannot compute"));
        return out;
    }

    // Drop the largest holder as the presumed pool vault or bonding curve.
    let non_pool = non_pool_amounts(&holdings, vault);

    // Without this guard, a mint whose entire supply sits in ONE account leaves
    // an empty vector, sums to 0.0%, and PASSES - a fail-open in the one module
    // whose whole contract is to fail closed.
    if non_pool.is_empty() {
        out.push(CheckResult::fail(
            "concentration",
            "only the pool/curve holds supply - no distribution to measure",
        ));
        return out;
    }

    let top10: u128 = non_pool.iter().take(10).sum();
    let pct = (top10 as f64 / facts.supply_raw as f64) * 100.0;

    let detail = format!("top 10 non-pool holders hold {pct:.1}% of supply");
    if pct <= cfg.max_top10_pct {
        out.push(CheckResult::pass("concentration", detail));
    } else if venue == Venue::PumpFun {
        // Not load-bearing this early in a bonding curve. Recorded, not fatal.
        out.push(CheckResult::warn(
            "concentration",
            format!("{detail} (advisory on pump.fun bonding curve)"),
        ));
    } else {
        out.push(CheckResult::fail(
            "concentration",
            format!("{detail}, limit {:.1}%", cfg.max_top10_pct),
        ));
    }

    out
}

/// Everything held by someone other than the pool.
///
/// Extracted so the selection rule can be tested directly - it is the part that
/// decides whether a whale gets counted, and it used to get that wrong.
fn non_pool_amounts(holdings: &[(String, u128)], vault: Option<&str>) -> Vec<u128> {
    match vault {
        // The pool account is known, so exclude exactly it and nothing else.
        Some(v) if !v.is_empty() => holdings
            .iter()
            .filter(|(addr, _)| addr != v)
            .map(|(_, amt)| *amt)
            .collect(),
        // The pool is unknown. Dropping the largest holder positionally would
        // silently exempt any whale that outranks the vault - the exact shape of
        // the rug this check exists to catch - so measure every account and let
        // the threshold decide. That is the fail-closed direction.
        _ => holdings.iter().map(|(_, amt)| *amt).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, u128)]) -> Vec<(String, u128)> {
        pairs.iter().map(|(a, n)| (a.to_string(), *n)).collect()
    }

    #[test]
    fn a_known_vault_is_excluded_exactly() {
        let holdings = h(&[("VAULT", 700), ("WHALE", 200), ("SMALL", 100)]);
        assert_eq!(non_pool_amounts(&holdings, Some("VAULT")), vec![200, 100]);
    }

    // REGRESSION: the pool used to be excluded POSITIONALLY, as the largest
    // account. When a dev wallet outranks the vault - which is exactly what a
    // Raydium/PumpSwap rug looks like - the whale became the one holder exempt
    // from the concentration measurement.
    #[test]
    fn a_whale_larger_than_the_vault_is_still_counted() {
        let holdings = h(&[("WHALE", 700), ("VAULT", 206), ("SMALL", 94)]);
        let non_pool = non_pool_amounts(&holdings, Some("VAULT"));
        assert!(
            non_pool.contains(&700),
            "the largest holder is the dev, not the pool, and must be measured"
        );
        assert!(!non_pool.contains(&206), "the real vault must be excluded");
    }

    #[test]
    fn an_unknown_vault_excludes_nothing() {
        // Fail-closed: with no way to identify the pool, measure everyone.
        let holdings = h(&[("A", 700), ("B", 200)]);
        assert_eq!(non_pool_amounts(&holdings, None), vec![700, 200]);
        assert_eq!(non_pool_amounts(&holdings, Some("")), vec![700, 200]);
    }

    #[test]
    fn a_vault_that_is_not_in_the_top_accounts_excludes_nothing() {
        let holdings = h(&[("A", 700), ("B", 200)]);
        assert_eq!(non_pool_amounts(&holdings, Some("ELSEWHERE")), vec![700, 200]);
    }
}
