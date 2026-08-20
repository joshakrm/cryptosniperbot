use anyhow::{bail, Result};
use async_trait::async_trait;

use super::Executor;
use crate::types::Fill;

/// Placeholder for real execution. Present so the wiring is stable; every
/// method refuses loudly rather than silently doing nothing.
///
/// What a real implementation has to add, roughly in order of difficulty:
///
///   1. Keypair loading from disk, never from config, never logged.
///   2. Jupiter /swap to build the transaction, or a direct venue-specific
///      instruction if you need the latency.
///   3. Compute budget + priority fee, sized dynamically off recent fees.
///   4. Jito bundle submission with a tip, or you lose every race that matters.
///   5. Signature confirmation with a real timeout, plus reconciliation for
///      the case where confirmation times out but the trade actually landed.
///   6. Balance reconciliation against on-chain state on every startup, so a
///      crash mid-trade does not leave the bot with a fictional position.
///
/// Step 5 is where most home-made bots lose money: they assume a timeout means
/// no fill, re-enter, and end up double-sized in a token that is already dumping.
#[allow(dead_code)] // deliberate placeholder; see module docs above
pub struct LiveExecutor;

#[async_trait]
impl Executor for LiveExecutor {
    fn name(&self) -> &'static str {
        "live"
    }

    async fn buy(&self, _mint: &str, _sol: f64, _price: f64) -> Result<Option<Fill>> {
        bail!("live execution is not implemented in this build")
    }

    async fn sell(&self, _mint: &str, _tokens: f64, _price: f64) -> Result<Option<Fill>> {
        bail!("live execution is not implemented in this build")
    }

    async fn balance_sol(&self) -> f64 {
        0.0
    }
}
