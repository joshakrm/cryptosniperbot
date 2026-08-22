use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tracing::debug;

use crate::config::ShadowConfig;
use crate::journal::Journal;
use crate::rpc::PriceSource;
use crate::types::Venue;

/// Follows candidates the bot did NOT trade, to build a control group.
///
/// Everything measured so far describes tokens that passed screening. That
/// cannot answer the only question that matters for profitability: do the
/// filters remove losers, or do they remove winners? A filter that rejects 79%
/// of candidates looks identical either way in the P&L.
///
/// So a small random sample of candidates is followed regardless of what the
/// screener decided, priced at a few sparse checkpoints, and journalled with the
/// verdict that was reached about them. Compare the outcome distribution of
/// rejected-by-X against the rest and X's value stops being a matter of opinion.
///
/// Cost is deliberately tiny: a few percent of candidates, four quotes each, all
/// at Mark priority so they queue behind live screening and never compete with a
/// trade decision. At 1750 launches/hour a 5% sample is roughly 0.1 requests per
/// second.
pub struct Shadow {
    prices: Arc<dyn PriceSource>,
    journal: Arc<Journal>,
    cfg: ShadowConfig,
}

/// Deterministic 64-bit FNV-1a. Sampling keyed on the mint rather than a random
/// number generator so a given token is always either in the sample or not -
/// re-running against the same stream produces the same control group.
fn hash_mint(mint: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in mint.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Shadow {
    pub fn new(prices: Arc<dyn PriceSource>, journal: Arc<Journal>, cfg: ShadowConfig) -> Self {
        Self { prices, journal, cfg }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && self.cfg.sample_pct > 0.0
    }

    /// Is this mint in the control sample?
    pub fn selects(&self, mint: &str) -> bool {
        if !self.enabled() {
            return false;
        }
        let bucket = hash_mint(mint) % 10_000;
        bucket < (self.cfg.sample_pct * 100.0) as u64
    }

    /// Follow a mint regardless of sampling.
    ///
    /// Used for positions that have just CLOSED. Marks otherwise stop at the
    /// exit, which means a replay can evaluate a tighter stop but never a looser
    /// one - there is no price data after the stop fired. That asymmetry would
    /// make every replay conclude that tighter is better, which is precisely the
    /// conclusion the data could not support. Closed positions are few (tens per
    /// day), so following all of them is cheap.
    pub fn track_after_exit(
        self: Arc<Self>,
        mint: String,
        venue: Venue,
        decimals: u8,
        verdict: ShadowVerdict,
    ) {
        if !self.cfg.enabled {
            return;
        }
        tokio::spawn(async move {
            self.follow(mint, venue, decimals, verdict, true).await;
        });
    }

    /// Follow one candidate to the end of its checkpoints, then journal it.
    ///
    /// Spawned rather than awaited: this outlives the screening decision by
    /// design, and nothing downstream should wait on it.
    pub fn track(
        self: Arc<Self>,
        mint: String,
        venue: Venue,
        decimals: u8,
        verdict: ShadowVerdict,
    ) {
        if !self.selects(&mint) {
            return;
        }
        tokio::spawn(async move {
            self.follow(mint, venue, decimals, verdict, false).await;
        });
    }

    async fn follow(
        &self,
        mint: String,
        venue: Venue,
        decimals: u8,
        verdict: ShadowVerdict,
        post_exit: bool,
    ) {
        // A fixed notional for every shadow, so the prices are comparable across
        // candidates rather than varying with an intended position size.
        let probe = self.cfg.probe_size_sol.max(0.001);

        // Getting a reference price takes a few attempts: a launch is usually not
        // routable the instant it is seen.
        //
        // Dropping the ones that never become routable would be a survivorship
        // bias straight into the control group - it would keep only the
        // candidates that were immediately tradeable, which is not a random
        // sample of anything. Measured: 1410 candidates at 5% should have given
        // ~70 records and gave 8. "Never routable" is an outcome, so it is
        // recorded as one.
        let mut reference = None;
        for attempt in 0..4u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
            if let Ok(Some(p)) = self.prices.mark(&mint, probe, decimals).await {
                if p > 0.0 {
                    reference = Some(p);
                    break;
                }
            }
        }

        let reference = match reference {
            Some(p) => p,
            None => {
                debug!(%mint, "shadow: never became routable");
                self.journal
                    .write(
                        "shadow",
                        json!({
                            "mint": mint,
                            "venue": venue,
                            "approved": verdict.approved,
                            "post_exit": post_exit,
                            "rejected_by": verdict.rejected_by,
                            "pool_sol": verdict.pool_sol,
                            "creator_share_pct": verdict.creator_share_pct,
                            "never_routable": true,
                            "observed_at": Utc::now().to_rfc3339(),
                        }),
                    )
                    .await;
                return;
            }
        };

        let mut marks = Vec::new();
        let mut elapsed = 0u64;
        for at in &self.cfg.marks_secs {
            if *at <= elapsed {
                continue;
            }
            tokio::time::sleep(Duration::from_secs(*at - elapsed)).await;
            elapsed = *at;

            let price = match self.prices.mark(&mint, probe, decimals).await {
                Ok(Some(p)) if p > 0.0 => Some(p),
                // A zero/negative price and no route are the same outcome, and
                // for a control group that outcome is data, not a missing value.
                Ok(_) => Some(0.0),
                Err(_) => None,
            };
            marks.push(json!({
                "t_s": at,
                "price_sol": price,
                "ret": price.map(|p| (p / reference) - 1.0),
            }));
        }

        let peak = marks
            .iter()
            .filter_map(|m| m.get("ret").and_then(|v| v.as_f64()))
            .fold(f64::NEG_INFINITY, f64::max);

        self.journal
            .write(
                "shadow",
                json!({
                    "mint": mint,
                    "venue": venue,
                    "approved": verdict.approved,
                    "post_exit": post_exit,
                    "rejected_by": verdict.rejected_by,
                    "pool_sol": verdict.pool_sol,
                    "creator_share_pct": verdict.creator_share_pct,
                    "never_routable": false,
                    "reference_price_sol": reference,
                    "marks": marks,
                    "peak_ret": if peak.is_finite() { Some(peak) } else { None },
                    "observed_at": Utc::now().to_rfc3339(),
                }),
            )
            .await;
    }
}

/// What the screener concluded about a candidate, carried alongside its outcome
/// so the two can be correlated later.
#[derive(Debug, Clone, Default)]
pub struct ShadowVerdict {
    pub approved: bool,
    pub rejected_by: Vec<String>,
    pub pool_sol: Option<f64>,
    pub creator_share_pct: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pct: f64, enabled: bool) -> ShadowConfig {
        ShadowConfig {
            enabled,
            sample_pct: pct,
            probe_size_sol: 0.1,
            marks_secs: vec![60, 300, 900],
        }
    }

    async fn shadow(pct: f64, enabled: bool) -> Shadow {
        struct Never;
        #[async_trait::async_trait]
        impl PriceSource for Never {
            async fn mark(&self, _: &str, _: f64, _: u8) -> anyhow::Result<Option<f64>> {
                Ok(None)
            }
        }
        // Journal is never written in these tests; a temp path is enough.
        let path = std::env::temp_dir().join("solsnipe-shadow-test.jsonl");
        let journal = Journal::open(&path).await.unwrap();
        Shadow::new(Arc::new(Never), Arc::new(journal), cfg(pct, enabled))
    }

    #[tokio::test]
    async fn sampling_is_deterministic_for_a_given_mint() {
        let s = shadow(50.0, true).await;
        let m = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
        let first = s.selects(m);
        for _ in 0..20 {
            assert_eq!(s.selects(m), first, "same mint must always land the same way");
        }
    }

    #[tokio::test]
    async fn the_sample_rate_is_roughly_honoured() {
        let s = shadow(10.0, true).await;
        let hits = (0..4000)
            .filter(|i| s.selects(&format!("mint{i}pumpXXXXXXXXXXXXXXXXXXXXXXXXXXXX")))
            .count();
        let pct = 100.0 * hits as f64 / 4000.0;
        assert!((pct - 10.0).abs() < 3.0, "sampled {pct:.1}%, expected about 10%");
    }

    #[tokio::test]
    async fn disabled_means_nothing_is_sampled() {
        let s = shadow(100.0, false).await;
        assert!(!s.selects("anything"));
        let off = shadow(0.0, true).await;
        assert!(!off.selects("anything"));
    }
}
