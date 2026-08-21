pub mod authority;
pub mod holders;
pub mod liquidity;
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

/// What the launch transaction revealed, beyond the mint itself.
///
/// Bundled rather than passed as loose parameters because both fields come
/// from the same place and both are legitimately absent when screening a mint
/// by hand from the CLI, where there is no transaction to read.
#[derive(Debug, Clone, Default)]
pub struct LaunchContext {
    /// The pool's token account for the base mint, so concentration can
    /// exclude exactly the pool instead of guessing positionally.
    pub vault: Option<String>,
    /// The pool's LP mint, when the launch created one.
    pub lp_mint: Option<String>,
    /// SOL placed in the pool at launch, read from the launch transaction.
    pub pool_sol: Option<f64>,
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
        ctx: &LaunchContext,
    ) -> ScreenReport {
        let started = Instant::now();

        let budget = std::time::Duration::from_millis(self.cfg.max_screen_ms);
        let report = tokio::time::timeout(budget, self.run_checks(mint, venue, ctx)).await;

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
        ctx: &LaunchContext,
    ) -> (Vec<CheckResult>, Option<i64>, Option<f64>) {
        let mut checks = Vec::new();

        // 0. Free gate first. Everything below costs a network call, and the
        // aggregator budget is what actually limits throughput, so the cheapest
        // possible rejection has to come before the expensive ones.
        checks.extend(liquidity::check_pool_size(&self.cfg, ctx.pool_sol));
        if disqualified(&checks) {
            return (checks, None, None);
        }

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
        checks.extend(
            holders::check(&self.rpc, &self.cfg, mint, &facts, venue, ctx.vault.as_deref()).await,
        );

        if disqualified(&checks) {
            return (checks, None, None);
        }

        // 3. Can the creator simply withdraw the pool? One RPC call, and it is
        // the only check that catches a rug which leaves the token untouched.
        // Runs before routing because routing costs three throttled quotes.
        checks.extend(liquidity::check(&self.rpc, &self.cfg, ctx.lp_mint.as_deref(), venue).await);

        if disqualified(&checks) {
            return (checks, None, None);
        }

        // 4. Routability, depth, and the round-trip honeypot test.
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
