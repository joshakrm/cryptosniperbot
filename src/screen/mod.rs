pub mod authority;
pub mod holders;
pub mod routing;

use std::time::Instant;
use tracing::debug;

use crate::config::{ScreenConfig, UnavailablePolicy};
use crate::rpc::{Jupiter, SolanaRpc};
use crate::types::{CheckResult, Pubkey, ScreenReport, Severity, Venue};

/// Any fatal failure so far. Checked between stages so a candidate that is
/// already disqualified does not cost further round trips on a latency path.
fn disqualified(checks: &[CheckResult]) -> bool {
    // Unavailable blocks too: a question we could not answer is not an answer.
    checks.iter().any(|c| !c.passed && c.severity != Severity::Advisory)
}

/// Facts about the mint account, read once and reused by later checks.
#[derive(Debug, Clone)]
pub struct MintFacts {
    pub decimals: u8,
    pub supply_raw: u128,
    pub is_token_2022: bool,
}

pub struct Screener {
    rpc: SolanaRpc,
    jup: Jupiter,
    cfg: ScreenConfig,
    wsol: String,
    /// The size we would actually buy. Quotes are taken at this size so the
    /// entry price reflects real, size-dependent impact.
    entry_size_sol: f64,
}

impl Screener {
    pub fn new(
        rpc: SolanaRpc,
        jup: Jupiter,
        cfg: ScreenConfig,
        wsol: String,
        entry_size_sol: f64,
    ) -> Self {
        Self { rpc, jup, cfg, wsol, entry_size_sol }
    }

    /// Run every check against a candidate mint.
    ///
    /// FAIL CLOSED is the governing rule here. Every check that cannot reach a
    /// confident "this is safe" returns a failure. An RPC timeout is a
    /// rejection, not a pass. The cost of a false reject is one missed trade;
    /// the cost of a false accept is the whole position.
    pub async fn screen(
        &self,
        mint: &Pubkey,
        venue: Venue,
        vault: Option<&str>,
    ) -> ScreenReport {
        let started = Instant::now();

        let budget = std::time::Duration::from_millis(self.cfg.max_screen_ms);
        let report = tokio::time::timeout(budget, self.run_checks(mint, venue, vault)).await;

        match report {
            Ok((checks, roundtrip, price)) => ScreenReport {
                mint: mint.clone(),
                checks: self.apply_unavailable_policy(checks),
                elapsed_ms: started.elapsed().as_millis() as u64,
                roundtrip_loss_bps: roundtrip,
                quoted_price_sol: price,
            },
            Err(_) => ScreenReport {
                mint: mint.clone(),
                checks: vec![CheckResult::unavailable(
                    "screen_timeout",
                    format!("did not finish within {}ms", self.cfg.max_screen_ms),
                )],
                elapsed_ms: started.elapsed().as_millis() as u64,
                roundtrip_loss_bps: None,
                quoted_price_sol: None,
            },
        }
    }

    /// Downgrade "could not check" to advisory when the operator has explicitly
    /// opted into trading through a flaky endpoint. Default leaves it blocking.
    fn apply_unavailable_policy(&self, mut checks: Vec<CheckResult>) -> Vec<CheckResult> {
        if self.cfg.treat_unavailable_as == UnavailablePolicy::Advisory {
            for c in checks.iter_mut() {
                if c.severity == Severity::Unavailable {
                    c.severity = Severity::Advisory;
                }
            }
        }
        checks
    }

    async fn run_checks(
        &self,
        mint: &Pubkey,
        venue: Venue,
        vault: Option<&str>,
    ) -> (Vec<CheckResult>, Option<i64>, Option<f64>) {
        let mut checks = Vec::new();

        // 1. Mint account: authorities and Token-2022 extensions.
        let (auth_checks, facts) = authority::check(&self.rpc, &self.cfg, mint).await;
        checks.extend(auth_checks);

        let facts = match facts {
            Some(f) => f,
            None => {
                // Without decimals we cannot size any quote, so nothing further
                // is knowable. Stop here rather than guessing.
                return (checks, None, None);
            }
        };
        debug!(?facts, %mint, "mint facts");

        // Bail out early if the mint is already disqualified: the remaining
        // checks cost external HTTP calls and latency we do not need to spend.
        if disqualified(&checks) {
            return (checks, None, None);
        }

        // 2. Holder distribution.
        checks.extend(holders::check(&self.rpc, &self.cfg, mint, &facts, venue, vault).await);

        // Same reasoning: routing below costs three more external round trips.
        if disqualified(&checks) {
            return (checks, None, None);
        }

        // 3. Routability, depth, and the round-trip honeypot test.
        let (route_checks, roundtrip, price) = routing::check(
            &self.jup,
            &self.cfg,
            self.entry_size_sol,
            &self.wsol,
            mint,
            &facts,
        )
        .await;
        checks.extend(route_checks);

        (checks, roundtrip, price)
    }
}
