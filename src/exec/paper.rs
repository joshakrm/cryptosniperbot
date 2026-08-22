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
/// first. You were not. What models that here:
///
///   latency_penalty_bps - the price already ran before your tx landed
///   fill_probability    - the fraction of races you lose outright
///   extra_slippage_bps  - measured residual, normally zero
///
/// WHAT IS NOT CHARGED HERE, AND WHY. Spread and size-dependent price impact
/// are already inside the quoted price this is handed. Both sides are quoted at
/// the real size: the entry at entry_size_sol (screen/routing.rs derives
/// entry_price from that quote's in/out amounts) and the mark at the tokens
/// actually held (rpc.rs JupiterPrices::mark). Charging a spread on top of a
/// quote that already crossed it counts the same cost twice.
///
/// That is not hypothetical. A `slippage_bps = 300` field used to be added here,
/// and over a 3-hour run the buy fill came out exactly 800.0 bps worse than the
/// screen's own quote on all 77 trades - min 800.0, median 800.0, max 800.0,
/// zero variance - while that same screen was recording a 366 bps round-trip on
/// the first of them. Of a 3.4695 SOL loss, 2.1846 SOL was this arithmetic.
///
/// It also quietly destroyed the thing the strategy selects on. A flat bps
/// penalty charges a 2 SOL pool and a 30 SOL pool identically, so it erased
/// exactly the depth dimension that pool_size exists to discriminate on -
/// while impact taken from the quote varies with depth for free.
///
/// Set what remains from measured reality once you have a few hundred journalled
/// signals, not from optimism. If your paper equity curve only works with
/// latency_penalty_bps near zero, you do not have a strategy - and removing a
/// double count is not the same thing as turning the penalty down.
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

        // You pay worse than the quote by the move that happened while your
        // transaction was in flight. Spread and impact are NOT added: the quote
        // was taken at this exact size and already contains both.
        let penalty =
            (self.cfg.latency_penalty_bps + self.cfg.extra_slippage_bps) as f64 / 10_000.0;
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

        // Latency still costs you on the way out, but you are not racing a
        // launch, so it is weighted at half the entry penalty. Impact is again
        // already in the mark, which was quoted at the tokens actually held.
        let penalty = (self.cfg.latency_penalty_bps as f64 / 2.0
            + self.cfg.extra_slippage_bps as f64)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(latency_bps: u64, extra_bps: u64) -> PaperConfig {
        let toml = format!(
            "starting_balance_sol = 10.0
priority_fee_sol = 0.0
latency_penalty_bps = {latency_bps}
extra_slippage_bps = {extra_bps}
fill_probability = 1.0
"
        );
        toml::from_str(&toml).expect("test paper config")
    }

    // The bug this file exists to prevent coming back. With latency at 500 the
    // entry must be 500 bps worse than the quote, not 800: the missing 300 was
    // a spread charged on a quote that had already crossed it.
    #[tokio::test]
    async fn entry_is_charged_latency_only_not_latency_plus_spread() {
        let ex = PaperExecutor::new(cfg(500, 0));
        let quote = 1.0e-7;
        let fill = ex.buy("M", 0.25, quote).await.unwrap().expect("filled");
        let bps = ((fill.price_sol / quote) - 1.0) * 10_000.0;
        assert!((bps - 500.0).abs() < 1e-6, "entry charged {bps} bps, expected 500");
    }

    // Exit is charged half the latency and nothing else, for the same reason:
    // the mark was quoted at the tokens actually held.
    #[tokio::test]
    async fn exit_is_charged_half_latency_only() {
        let ex = PaperExecutor::new(cfg(500, 0));
        let quote = 1.0e-7;
        let fill = ex.sell("M", 1_000_000.0, quote).await.unwrap().expect("filled");
        let bps = (1.0 - (fill.price_sol / quote)) * 10_000.0;
        assert!((bps - 250.0).abs() < 1e-6, "exit charged {bps} bps, expected 250");
    }

    // The escape hatch still works, so a measured live-vs-paper gap can be
    // modelled without reintroducing the double count by default.
    #[tokio::test]
    async fn extra_slippage_is_added_on_top_when_asked_for() {
        let ex = PaperExecutor::new(cfg(500, 300));
        let quote = 1.0e-7;
        let fill = ex.buy("M", 0.25, quote).await.unwrap().expect("filled");
        let bps = ((fill.price_sol / quote) - 1.0) * 10_000.0;
        assert!((bps - 800.0).abs() < 1e-6, "expected opt-in 800 bps, got {bps}");
    }

    // Zero cost must mean zero cost: any residual here would be an unexamined
    // constant sitting under every P&L figure the project produces.
    #[tokio::test]
    async fn no_penalty_means_the_fill_is_the_quote() {
        let ex = PaperExecutor::new(cfg(0, 0));
        let quote = 1.0e-7;
        let buy = ex.buy("M", 0.25, quote).await.unwrap().expect("filled");
        assert!((buy.price_sol - quote).abs() < 1e-18, "buy moved off the quote");
        let sell = ex.sell("M", 1_000_000.0, quote).await.unwrap().expect("filled");
        assert!((sell.price_sol - quote).abs() < 1e-18, "sell moved off the quote");
    }

    // A quote already reflects depth, so two different quotes must produce
    // proportionally different fills - the property a flat bps penalty destroyed.
    #[tokio::test]
    async fn a_worse_quote_produces_a_proportionally_worse_fill() {
        let ex = PaperExecutor::new(cfg(500, 0));
        let deep = ex.buy("M", 0.25, 1.0e-7).await.unwrap().expect("filled");
        let thin = ex.buy("M", 0.25, 2.0e-7).await.unwrap().expect("filled");
        let ratio = thin.price_sol / deep.price_sol;
        assert!((ratio - 2.0).abs() < 1e-9, "impact from the quote was not preserved: {ratio}");
    }
}
