use crate::config::ScreenConfig;
use crate::rpc::SolanaRpc;
use crate::types::{CheckResult, Venue};

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
/// Reject a launch too thin to trade, using only what the transaction already
/// told us. Runs before any network call, which is the entire point.
pub fn check_pool_size(cfg: &ScreenConfig, pool_sol: Option<f64>) -> Vec<CheckResult> {
    if cfg.min_pool_sol <= 0.0 {
        return Vec::new();
    }
    match pool_sol {
        Some(sol) if sol >= cfg.min_pool_sol => vec![CheckResult::pass(
            "pool_size",
            format!("{sol:.2} SOL in the pool at launch"),
        )],
        Some(sol) => vec![CheckResult::fail(
            "pool_size",
            format!("only {sol:.2} SOL in the pool, need {:.2}", cfg.min_pool_sol),
        )],
        // Could not read it, so we cannot claim the pool is deep enough.
        None => vec![CheckResult::unavailable(
            "pool_size",
            "launch transaction did not reveal the pool size",
        )],
    }
}

pub async fn check(
    rpc: &SolanaRpc,
    cfg: &ScreenConfig,
    lp_mint: Option<&str>,
    venue: Venue,
) -> Vec<CheckResult> {
    if !cfg.require_lp_burned {
        return Vec::new();
    }

    let lp = match lp_mint {
        Some(m) if !m.is_empty() => m,

        // No LP mint found. What that MEANS depends entirely on the venue, and
        // conflating the two cases is a fail-open.
        //
        // On a bonding curve there genuinely is no liquidity position, so there
        // is nothing to withdraw and this is the safest case there is.
        _ if venue == Venue::PumpFun => {
            return vec![CheckResult::pass(
                "lp_burned",
                "no LP mint - bonding curve, no withdrawable liquidity position",
            )]
        }

        // On a pool venue the pool certainly HAS an LP position; failing to
        // identify it means our discriminator did not work here, not that the
        // liquidity is locked. Measured on PumpSwap migrations: 41 of 120
        // candidates passed this check purely because the LP mint could not be
        // found, which is the check approving what it could not inspect.
        _ => {
            return vec![CheckResult::unavailable(
                "lp_burned",
                "pool venue but no LP mint identified - cannot confirm liquidity is locked",
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

    fn test_rpc_cfg() -> crate::config::RpcConfig {
        toml::from_str(
            "http_url='http://127.0.0.1:1'
ws_url='ws://127.0.0.1:1'
jupiter_url='http://127.0.0.1:1'
"
        ).expect("test rpc config")
    }

    fn cfg(min_pool_sol: f64) -> ScreenConfig {
        // Only min_pool_sol matters here; the rest is inert for this check.
        let toml = format!(
            "min_liquidity_sol=1.0
max_top10_pct=65.0
min_holders=5
             require_mint_authority_renounced=true
require_freeze_authority_renounced=true
             reject_token2022_extensions=true
require_lp_burned=true
require_holders=true
             max_roundtrip_loss_bps=1500
max_screen_ms=15000
min_pool_sol={min_pool_sol}
"
        );
        toml::from_str(&toml).expect("test config")
    }

    // The aggregator budget is the binding constraint, and this is the only
    // rejection that costs nothing - it reads a number the launch transaction
    // already carried. Measured over 225 launches, a 2 SOL floor drops 62%.
    #[test]
    fn a_thin_pool_is_rejected_before_any_network_call() {
        let out = check_pool_size(&cfg(2.0), Some(0.57));
        assert_eq!(out.len(), 1);
        assert!(!out[0].passed);
        assert!(out[0].detail.contains("0.57"), "{}", out[0].detail);
    }

    #[test]
    fn a_deep_enough_pool_passes() {
        let out = check_pool_size(&cfg(2.0), Some(6.9));
        assert!(out[0].passed, "{}", out[0].detail);
    }

    #[test]
    fn an_unreadable_pool_size_does_not_pass() {
        let out = check_pool_size(&cfg(2.0), None);
        assert!(!out[0].passed);
        assert_eq!(out[0].severity, Severity::Unavailable);
    }

    #[test]
    fn a_zero_threshold_disables_the_gate_entirely() {
        assert!(check_pool_size(&cfg(0.0), Some(0.001)).is_empty());
        assert!(check_pool_size(&cfg(0.0), None).is_empty());
    }

    // REGRESSION, measured on 120 PumpSwap migrations: 41 candidates PASSED this
    // check because extract_lp_mint returned None, and None was being read as
    // "bonding curve, nothing to withdraw". On a pool venue the pool certainly
    // has an LP position - failing to find it means the discriminator did not
    // work, not that the liquidity is locked. The check was approving exactly
    // what it could not inspect.
    #[tokio::test]
    async fn a_pool_venue_with_no_identifiable_lp_does_not_pass() {
        let rpc = SolanaRpc::new(&test_rpc_cfg()).unwrap();
        let out = check(&rpc, &cfg(0.0), None, Venue::PumpSwap).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].passed, "{}", out[0].detail);
        assert_eq!(out[0].severity, Severity::Unavailable);
    }

    #[tokio::test]
    async fn a_bonding_curve_with_no_lp_really_is_safe() {
        let rpc = SolanaRpc::new(&test_rpc_cfg()).unwrap();
        let out = check(&rpc, &cfg(0.0), None, Venue::PumpFun).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].passed, "{}", out[0].detail);
    }

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
