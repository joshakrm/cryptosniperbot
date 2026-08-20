use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Base58 pubkey. Kept as a String deliberately: this build talks raw JSON-RPC
/// and does not pull in the solana-sdk dependency tree.
pub type Pubkey = String;

pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    PumpFun,
    PumpSwap,
    RaydiumAmmV4,
    RaydiumCpmm,
    Unknown,
}

impl Venue {
    pub fn from_label(s: &str) -> Venue {
        match s {
            "pump_fun" => Venue::PumpFun,
            "pump_swap" => Venue::PumpSwap,
            "raydium_amm_v4" => Venue::RaydiumAmmV4,
            "raydium_cpmm" => Venue::RaydiumCpmm,
            _ => Venue::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The check ran and the answer was disqualifying.
    Fatal,
    /// The check ran and the answer was concerning but tolerable.
    Advisory,
    /// The check could not run at all - RPC error, timeout, throttling.
    ///
    /// Deliberately distinct from Fatal. Both block the trade by default, but
    /// they demand opposite responses from the operator: Fatal means the
    /// screening is working, Unavailable means your endpoint is broken. Folding
    /// them together makes a throttled RPC look like a wall of dangerous
    /// tokens, which is exactly what a live run on a public RPC looked like.
    Unavailable,
}

// Serialize only: the `name: &'static str` field below cannot satisfy
// Deserialize<'de> for a generic 'de, and nothing here is ever read back
// (stats re-parses the journal as raw Value).
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: &'static str,
    pub passed: bool,
    pub severity: Severity,
    pub detail: String,
}

impl CheckResult {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: true, severity: Severity::Fatal, detail: detail.into() }
    }
    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: false, severity: Severity::Fatal, detail: detail.into() }
    }
    pub fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: false, severity: Severity::Advisory, detail: detail.into() }
    }
    /// The check could not be performed. Blocks the trade by default: an
    /// unanswered question about safety is not a safe answer.
    pub fn unavailable(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: false, severity: Severity::Unavailable, detail: detail.into() }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenReport {
    pub mint: Pubkey,
    pub checks: Vec<CheckResult>,
    pub elapsed_ms: u64,
    /// Round-trip quote loss in basis points, if we got that far.
    pub roundtrip_loss_bps: Option<i64>,
    /// Entry price in SOL per token, from the buy-side quote.
    pub quoted_price_sol: Option<f64>,
}

impl ScreenReport {
    pub fn approved(&self) -> bool {
        !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|c| c.passed || c.severity == Severity::Advisory)
    }

    /// Everything blocking the trade: checks that failed, and checks that could
    /// not run. Advisory failures are excluded - they do not block.
    pub fn rejections(&self) -> Vec<&CheckResult> {
        self.checks
            .iter()
            .filter(|c| !c.passed && c.severity != Severity::Advisory)
            .collect()
    }

    /// True when nothing is actually wrong with the token and the only thing
    /// standing in the way is our own inability to check it.
    pub fn blocked_only_by_unavailable(&self) -> bool {
        let blocking = self.rejections();
        !blocking.is_empty() && blocking.iter().all(|c| c.severity == Severity::Unavailable)
    }

    pub fn summary(&self) -> String {
        if self.approved() {
            format!("APPROVED in {}ms", self.elapsed_ms)
        } else if self.blocked_only_by_unavailable() {
            let reasons: Vec<String> = self
                .rejections()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect();
            format!("UNCHECKABLE ({}) [{}ms]", reasons.join("; "), self.elapsed_ms)
        } else {
            let reasons: Vec<String> = self
                .rejections()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect();
            format!("REJECTED ({}) [{}ms]", reasons.join("; "), self.elapsed_ms)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    TrailingStop,
    MaxHold,
    KillSwitch,
    Unroutable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub mint: Pubkey,
    pub is_buy: bool,
    /// SOL spent (buy) or received (sell), net of modelled costs.
    pub sol_amount: f64,
    /// Token quantity, in whole tokens.
    pub token_amount: f64,
    /// Effective price in SOL per token, after slippage and latency penalty.
    pub price_sol: f64,
    pub fees_sol: f64,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub mint: Pubkey,
    pub venue: Venue,
    pub decimals: u8,
    pub opened_at: DateTime<Utc>,
    pub entry_price_sol: f64,
    pub tokens_held: f64,
    pub tokens_initial: f64,
    pub sol_invested: f64,
    pub sol_realised: f64,
    pub peak_price_sol: f64,
    /// Index of the next take-profit rung to fire.
    pub next_rung: usize,
    /// Consecutive sweeps where a sell route could not be found. A rug and a
    /// hiccup at the aggregator look identical on any single sweep, so we
    /// require repeated confirmation before writing a position off.
    pub unroutable_strikes: u8,
    /// The last price we actually observed. Used to value a position that has
    /// outlived its hold window while unmarkable - a stale number, but a real
    /// one, and better than holding a slot forever.
    pub last_price_sol: Option<f64>,
}

impl Position {
    pub fn unrealised_bps(&self, current_price: f64) -> i64 {
        if self.entry_price_sol <= 0.0 {
            return 0;
        }
        (((current_price / self.entry_price_sol) - 1.0) * 10_000.0) as i64
    }

    pub fn drawdown_from_peak_bps(&self, current_price: f64) -> i64 {
        if self.peak_price_sol <= 0.0 {
            return 0;
        }
        (((self.peak_price_sol - current_price) / self.peak_price_sol) * 10_000.0) as i64
    }

    /// Realised + open value, minus what went in.
    pub fn pnl_sol(&self, current_price: f64) -> f64 {
        self.sol_realised + (self.tokens_held * current_price) - self.sol_invested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn pos() -> Position {
        Position {
            mint: "M".into(),
            venue: Venue::PumpFun,
            decimals: 6,
            opened_at: Utc::now(),
            entry_price_sol: 1.0,
            tokens_held: 100.0,
            tokens_initial: 100.0,
            sol_invested: 100.0,
            sol_realised: 0.0,
            peak_price_sol: 1.0,
            next_rung: 0,
            unroutable_strikes: 0,
            last_price_sol: None,
        }
    }

    #[test]
    fn unrealised_bps_tracks_price() {
        assert_eq!(pos().unrealised_bps(1.5), 5_000);
        assert_eq!(pos().unrealised_bps(0.5), -5_000);
        assert_eq!(pos().unrealised_bps(1.0), 0);
    }

    #[test]
    fn zero_entry_price_cannot_divide_by_zero() {
        let mut p = pos();
        p.entry_price_sol = 0.0;
        assert_eq!(p.unrealised_bps(5.0), 0);
        p.peak_price_sol = 0.0;
        assert_eq!(p.drawdown_from_peak_bps(1.0), 0);
    }

    #[test]
    fn drawdown_is_measured_from_the_peak() {
        let mut p = pos();
        p.peak_price_sol = 2.0;
        assert_eq!(p.drawdown_from_peak_bps(1.5), 2_500);
        assert_eq!(p.drawdown_from_peak_bps(2.0), 0);
    }

    #[test]
    fn pnl_counts_realised_plus_open_value_minus_cost() {
        let mut p = pos();
        p.tokens_held = 50.0;
        p.sol_realised = 80.0; // sold half for 80
        // 80 realised + 50 tokens at 1.5 = 155, less 100 invested
        assert!((p.pnl_sol(1.5) - 55.0).abs() < 1e-9);
    }

    #[test]
    fn an_advisory_failure_still_approves() {
        let r = ScreenReport {
            mint: "M".into(),
            checks: vec![
                CheckResult::pass("a", "ok"),
                CheckResult::warn("concentration", "high but advisory"),
            ],
            elapsed_ms: 1,
            roundtrip_loss_bps: None,
            quoted_price_sol: None,
        };
        assert!(r.approved());
        assert!(r.rejections().is_empty());
    }

    #[test]
    fn a_fatal_failure_rejects() {
        let r = ScreenReport {
            mint: "M".into(),
            checks: vec![CheckResult::pass("a", "ok"), CheckResult::fail("b", "nope")],
            elapsed_ms: 1,
            roundtrip_loss_bps: None,
            quoted_price_sol: None,
        };
        assert!(!r.approved());
        assert_eq!(r.rejections().len(), 1);
    }

    #[test]
    fn an_empty_report_never_approves() {
        // Fail closed: no checks ran means nothing was proven safe.
        let r = ScreenReport {
            mint: "M".into(),
            checks: vec![],
            elapsed_ms: 0,
            roundtrip_loss_bps: None,
            quoted_price_sol: None,
        };
        assert!(!r.approved());
    }
}
