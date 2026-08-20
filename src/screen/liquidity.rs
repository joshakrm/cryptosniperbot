use crate::config::ScreenConfig;
use crate::rpc::SolanaRpc;
use crate::types::CheckResult;

/// Can the creator withdraw the liquidity you are about to trade against?
///
/// Every other check in this module asks whether the *token* is safe. This one
/// asks whether the *pool* is, and it is the only check that catches a creator
/// who leaves the token completely alone and simply removes the money.
///
/// The signal is the LP mint's supply:
///
///   no LP mint  - a bonding curve, not a pool. There is no liquidity position
///                 in existence, so there is nothing to withdraw. Safest case.
///   supply 0    - the LP tokens were burned. Nobody can redeem the pool.
///   supply > 0  - somebody holds a redeemable claim on the pool.
///
/// Measured on live PumpSwap launches: 3 burned, 1 with 4.1 trillion LP units
/// outstanding, 2 curve-style. So this discriminates, unlike metadata
/// mutability, which was identical on 25 of 25 sampled mints and could
/// therefore never reject anything.
///
/// KNOWN FALSE NEGATIVE: a pool whose LP is locked in a third-party locker
/// rather than burned still reports a non-zero supply and is rejected here.
/// That is the fail-closed direction and it is the intended trade-off -
/// verifying a locker would mean resolving each holder's owner, and we cannot
/// enumerate every locker program anyway.
pub async fn check(
    rpc: &SolanaRpc,
    cfg: &ScreenConfig,
    lp_mint: Option<&str>,
) -> Vec<CheckResult> {
    if !cfg.require_lp_burned {
        return Vec::new();
    }

    let lp = match lp_mint {
        Some(m) if !m.is_empty() => m,
        // No LP mint in the launch transaction: a bonding curve. Nothing to pull.
        _ => {
            return vec![CheckResult::pass(
                "lp_burned",
                "no LP mint - bonding curve, no withdrawable liquidity position",
            )]
        }
    };

    let supply = match rpc.get_token_supply(&lp.to_string()).await {
        Ok(v) => v,
        Err(e) => {
            return vec![CheckResult::unavailable(
                "lp_burned",
                format!("could not read LP supply: {e}"),
            )]
        }
    };

    let raw = supply
        .get("value")
        .and_then(|v| v.get("amount"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u128>().ok());

    vec![verdict(lp, raw)]
}

/// Supply to verdict. Separated so the rule can be tested without a network.
fn verdict(lp: &str, raw_supply: Option<u128>) -> CheckResult {
    let short = &lp[..lp.len().min(8)];
    match raw_supply {
        Some(0) => CheckResult::pass(
            "lp_burned",
            format!("LP {short} fully burned - liquidity locked"),
        ),
        Some(n) => CheckResult::fail(
            "lp_burned",
            format!("{n} LP units outstanding on {short} - the creator can withdraw the pool"),
        ),
        // Fail closed: an unparseable supply is not evidence of a burn.
        None => CheckResult::unavailable("lp_burned", "LP supply response was not parseable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Severity;

    #[test]
    fn a_burned_lp_passes() {
        let c = verdict("GLu7LTLhqo1d3Nne", Some(0));
        assert!(c.passed, "{}", c.detail);
    }

    // REGRESSION shape: measured live, 1 of 4 PumpSwap pools had 4.1 trillion
    // LP units outstanding - liquidity the creator could withdraw at will,
    // which every other check in this crate passes without comment.
    #[test]
    fn outstanding_lp_is_rejected() {
        let c = verdict("E45Zr2LDa5orpptd", Some(4_122_347_331_863));
        assert!(!c.passed);
        assert_eq!(c.severity, Severity::Fatal);
        assert!(c.detail.contains("withdraw"), "{}", c.detail);
    }

    #[test]
    fn an_unreadable_supply_does_not_count_as_burned() {
        let c = verdict("whatever", None);
        assert!(!c.passed);
        assert_eq!(c.severity, Severity::Unavailable);
    }

    #[test]
    fn a_short_mint_string_does_not_panic_on_slicing() {
        let _ = verdict("ab", Some(0));
        let _ = verdict("", Some(1));
    }
}
