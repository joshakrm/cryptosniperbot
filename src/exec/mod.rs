pub mod live;
pub mod paper;

use anyhow::Result;
use async_trait::async_trait;

use crate::types::Fill;

/// Anything that can put on and take off a position.
///
/// `Ok(None)` means "this order would not have filled at all" - the paper
/// executor uses it to model the snipes you lose to faster bots, which is the
/// difference between a backtest and a fantasy.
#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;

    async fn buy(
        &self,
        mint: &str,
        sol_amount: f64,
        quoted_price_sol: f64,
    ) -> Result<Option<Fill>>;

    async fn sell(
        &self,
        mint: &str,
        token_amount: f64,
        quoted_price_sol: f64,
    ) -> Result<Option<Fill>>;

    async fn balance_sol(&self) -> f64;
}
