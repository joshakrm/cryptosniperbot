use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::Executor;
use crate::config::PaperConfig;
use crate::types::Fill;

/// Simulated execution with a cost model that is deliberately pessimistic.
///
/// The failure mode of every home-made sniper backtest is assuming you were
/// first. You were not. Three separate penalties model that here:
///
///   slippage_bps        - the spread you actually cross
///   latency_penalty_bps - the price already ran before your tx landed
///   fill_probability    - the fraction of races you lose outright
///
/// Set them from measured reality once you have a few hundred journalled
/// signals, not from optimism. If your paper equity curve only works with
/// latency_penalty_bps near zero, you do not have a strategy.
pub struct PaperExecutor {
    cfg: PaperConfig,
    state: Mutex<PaperState>,
}

struct PaperState {
    balance_sol: f64,
    rng: u64,
    attempted: u64,
    landed: u64,
}

impl PaperExecutor {
    pub fn new(cfg: PaperConfig) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545F4914F6CDD1D)
            | 1;
        let balance = cfg.starting_balance_sol;
        Self {
            cfg,
            state: Mutex::new(PaperState {
                balance_sol: balance,
                rng: seed,
                attempted: 0,
                landed: 0,
            }),
        }
    }

    pub async fn stats(&self) -> (f64, u64, u64) {
        let st = self.state.lock().await;
        (st.balance_sol, st.attempted, st.landed)
    }
}

/// xorshift64, so the crate does not pull in `rand` for one coin flip.
fn next_f64(state: &mut u64) -> f64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[async_trait]
impl Executor for PaperExecutor {
    fn name(&self) -> &'static str {
        "paper"
    }

    async fn buy(
        &self,
        mint: &str,
        sol_amount: f64,
        quoted_price_sol: f64,
    ) -> Result<Option<Fill>> {
        let mut st = self.state.lock().await;
        st.attempted += 1;

        if quoted_price_sol <= 0.0 {
            warn!(%mint, "buy skipped: non-positive quoted price");
            return Ok(None);
        }

        // Did we win the race at all?
        let roll = next_f64(&mut st.rng);
        if roll > self.cfg.fill_probability {
            info!(%mint, "simulated miss: another bot landed first");
            return Ok(None);
        }

        let fees = self.cfg.priority_fee_sol;
        if st.balance_sol < sol_amount + fees {
            warn!(%mint, balance = st.balance_sol, "buy skipped: insufficient paper balance");
            return Ok(None);
        }

        // You pay worse than the quote: spread crossed, plus the move that
        // happened while your transaction was in flight.
        let penalty = (self.cfg.slippage_bps + self.cfg.latency_penalty_bps) as f64 / 10_000.0;
        let effective_price = quoted_price_sol * (1.0 + penalty);
        let tokens = sol_amount / effective_price;

        st.balance_sol -= sol_amount + fees;
        st.landed += 1;

        Ok(Some(Fill {
            mint: mint.to_string(),
            is_buy: true,
            sol_amount,
            token_amount: tokens,
            price_sol: effective_price,
            fees_sol: fees,
            at: Utc::now(),
        }))
    }

    async fn sell(
        &self,
        mint: &str,
        token_amount: f64,
        quoted_price_sol: f64,
    ) -> Result<Option<Fill>> {
        let mut st = self.state.lock().await;

        if quoted_price_sol <= 0.0 || token_amount <= 0.0 {
            return Ok(None);
        }

        // Exits cross the spread too. Latency still costs you, but you are not
        // racing a launch, so it is weighted at half the entry penalty.
        let penalty = (self.cfg.slippage_bps as f64 + self.cfg.latency_penalty_bps as f64 / 2.0)
            / 10_000.0;
        let effective_price = quoted_price_sol * (1.0 - penalty).max(0.0);

        let fees = self.cfg.priority_fee_sol;
        let gross = token_amount * effective_price;
        // Charged unconditionally, including on exits that lose money. Clamping
        // at zero would forgive the fee on exactly the trades that hurt most,
        // which biases paper PnL upward precisely on the rugs.
        let net = gross - fees;

        st.balance_sol += net;

        Ok(Some(Fill {
            mint: mint.to_string(),
            is_buy: false,
            sol_amount: net,
            token_amount,
            price_sol: effective_price,
            fees_sol: fees,
            at: Utc::now(),
        }))
    }

    async fn balance_sol(&self) -> f64 {
        self.state.lock().await.balance_sol
    }
}
