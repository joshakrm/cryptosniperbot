use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::ExitConfig;
use crate::exec::Executor;
use crate::journal::Journal;
use crate::risk::RiskManager;
use crate::rpc::PriceSource;
use crate::types::{ExitReason, Position, Venue};

/// Consecutive sweeps with no sell route before a position is written off.
///
/// One failed quote is not evidence of a rug: it is equally consistent with a
/// hiccup at the aggregator. Booking a total loss on a single data point is how
/// you turn somebody else's outage into your realised loss.
const UNROUTABLE_STRIKES: u8 = 3;

/// What a mark attempt actually told us. The three cases are genuinely
/// different and collapsing them is a money bug: `NoRoute` is evidence about
/// the token, `Unknown` is the absence of evidence about anything.
enum Mark {
    Price(f64),
    NoRoute,
    Unknown,
}

pub struct PositionManager {
    prices: Arc<dyn PriceSource>,
    exec: Arc<dyn Executor>,
    cfg: ExitConfig,
    risk: Arc<RiskManager>,
    journal: Arc<Journal>,
    positions: Mutex<HashMap<String, Position>>,
}

impl PositionManager {
    pub fn new(
        prices: Arc<dyn PriceSource>,
        exec: Arc<dyn Executor>,
        cfg: ExitConfig,
        risk: Arc<RiskManager>,
        journal: Arc<Journal>,
    ) -> Self {
        Self {
            prices,
            exec,
            cfg,
            risk,
            journal,
            positions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn is_open(&self, mint: &str) -> bool {
        self.positions.lock().await.contains_key(mint)
    }

    pub async fn open_count(&self) -> usize {
        self.positions.lock().await.len()
    }

    pub async fn open(
        &self,
        mint: String,
        venue: Venue,
        decimals: u8,
        entry_price: f64,
        tokens: f64,
        sol_invested: f64,
    ) {
        let pos = Position {
            mint: mint.clone(),
            venue,
            decimals,
            opened_at: Utc::now(),
            entry_price_sol: entry_price,
            tokens_held: tokens,
            tokens_initial: tokens,
            sol_invested,
            sol_realised: 0.0,
            peak_price_sol: entry_price,
            next_rung: 0,
            unroutable_strikes: 0,
            last_price_sol: None,
        };
        self.journal.write_typed("position_open", &pos).await;
        self.positions.lock().await.insert(mint, pos);
    }

    /// Poll marks and apply exit rules until the process ends.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(self.cfg.poll_interval_ms));
        loop {
            tick.tick().await;
            if let Err(e) = self.sweep().await {
                warn!(error = %e, "position sweep failed");
            }
        }
    }

    async fn sweep(&self) -> Result<()> {
        let snapshot: Vec<Position> = {
            let map = self.positions.lock().await;
            map.values().cloned().collect()
        };
        if snapshot.is_empty() {
            return Ok(());
        }

        let kill = self.risk.kill_switch_engaged();

        for pos in snapshot {
            let price = match self.mark_price(&pos).await {
                Mark::Price(p) if p > 0.0 => {
                    self.record_mark(&pos.mint, p, pos.unroutable_strikes > 0).await;
                    p
                }
                // A zero price is as unsellable as no route at all.
                Mark::Price(_) | Mark::NoRoute => {
                    let strikes = self.add_strike(&pos.mint).await;
                    if strikes >= UNROUTABLE_STRIKES {
                        // Genuinely unsellable, so it really is worth zero.
                        warn!(
                            mint = %pos.mint,
                            strikes,
                            "no sell route on {UNROUTABLE_STRIKES} consecutive sweeps - writing off"
                        );
                        self.force_close(&pos.mint, 0.0, ExitReason::Unroutable).await;
                    } else {
                        warn!(mint = %pos.mint, strikes, "no sell route this sweep");
                    }
                    continue;
                }
                Mark::Unknown => {
                    // A transport failure still tells us nothing about the
                    // token, so no price-based rule may fire. But the CLOCK
                    // keeps running, and simply skipping the position leaves it
                    // open forever: it holds a risk slot and keeps spending mark
                    // budget indefinitely.
                    //
                    // Measured: 10 positions held 58 minutes against a 15 minute
                    // max_hold, and the budget they consumed starved screening
                    // so completely that approvals went to zero for the last 50
                    // minutes of a live hour. Unmanageable positions have to be
                    // let go of.
                    let age = (Utc::now() - pos.opened_at).num_seconds();
                    if age >= self.cfg.max_hold_secs {
                        let stale = pos.last_price_sol.unwrap_or(pos.entry_price_sol);
                        warn!(
                            mint = %pos.mint,
                            age,
                            stale_price = stale,
                            "past max hold and unmarkable - closing at last observed price"
                        );
                        self.force_close(&pos.mint, stale, ExitReason::MaxHold).await;
                    }
                    continue;
                }
            };

            if kill {
                info!(mint = %pos.mint, "kill switch engaged - flattening position");
                self.exit_all(&pos, price, ExitReason::KillSwitch).await;
                continue;
            }

            self.evaluate(&pos, price).await;
        }
        Ok(())
    }

    async fn mark_price(&self, pos: &Position) -> Mark {
        match self
            .prices
            .mark(&pos.mint, pos.tokens_held, pos.decimals)
            .await
        {
            Ok(Some(p)) => Mark::Price(p),
            Ok(None) => Mark::NoRoute,
            Err(e) => {
                warn!(mint = %pos.mint, error = %e, "mark lookup failed - holding");
                Mark::Unknown
            }
        }
    }

    async fn add_strike(&self, mint: &str) -> u8 {
        let mut map = self.positions.lock().await;
        match map.get_mut(mint) {
            Some(p) => {
                p.unroutable_strikes = p.unroutable_strikes.saturating_add(1);
                p.unroutable_strikes
            }
            None => 0,
        }
    }

    /// Remember the last real price, and clear any accumulated strikes.
    async fn record_mark(&self, mint: &str, price: f64, had_strikes: bool) {
        let mut map = self.positions.lock().await;
        if let Some(p) = map.get_mut(mint) {
            p.last_price_sol = Some(price);
            if had_strikes {
                p.unroutable_strikes = 0;
            }
        }
    }

    async fn evaluate(&self, pos: &Position, price: f64) {
        // Track the peak for the trailing stop.
        let mut latest = pos.clone();
        if price > pos.peak_price_sol {
            latest.peak_price_sol = price;
            let mut map = self.positions.lock().await;
            if let Some(p) = map.get_mut(&pos.mint) {
                p.peak_price_sol = price;
            }
        }

        let age = (Utc::now() - pos.opened_at).num_seconds();
        if let Some((qty, reason)) = decide_exit(&latest, price, age, &self.cfg) {
            info!(
                mint = %pos.mint,
                gain_bps = latest.unrealised_bps(price),
                ?reason,
                "exit triggered"
            );
            self.partial_exit(pos, qty, price, reason).await;
        }
    }

    async fn exit_all(&self, pos: &Position, price: f64, reason: ExitReason) {
        self.partial_exit(pos, pos.tokens_held, price, reason).await;
    }

    async fn partial_exit(&self, pos: &Position, qty: f64, price: f64, reason: ExitReason) {
        if qty <= 0.0 {
            return;
        }

        let fill = match self.exec.sell(&pos.mint, qty, price).await {
            Ok(Some(f)) => f,
            Ok(None) => {
                warn!(mint = %pos.mint, "sell did not fill");
                return;
            }
            Err(e) => {
                warn!(mint = %pos.mint, error = %e, "sell failed");
                return;
            }
        };

        self.journal
            .write(
                "fill_sell",
                json!({
                    "mint": pos.mint,
                    "reason": reason,
                    "tokens": fill.token_amount,
                    "sol": fill.sol_amount,
                    "price": fill.price_sol,
                }),
            )
            .await;

        let mut closed = false;
        let mut pnl = 0.0;

        {
            let mut map = self.positions.lock().await;
            if let Some(p) = map.get_mut(&pos.mint) {
                p.tokens_held = (p.tokens_held - fill.token_amount).max(0.0);
                p.sol_realised += fill.sol_amount;
                if matches!(reason, ExitReason::TakeProfit) {
                    p.next_rung += 1;
                }

                // Dust is measured in VALUE, not token count. A position that
                // ran 100x can hold 0.1% of its tokens and still be worth a
                // tenth of the stake - abandoning that is not rounding, it is
                // throwing money away.
                let residual_sol = p.tokens_held * fill.price_sol;
                if p.tokens_held <= 0.0 || residual_sol <= self.cfg.dust_value_sol {
                    closed = true;
                    // Marked at zero, not at the last fill price: we are walking
                    // away from this residual and will never realise it. Booking
                    // it at a price we decline to trade at would inflate every
                    // paper result by exactly the amount we abandon, and would
                    // disagree with force_close, which already marks at zero.
                    pnl = p.pnl_sol(0.0);
                }
            }
            if closed {
                map.remove(&pos.mint);
            }
        }

        if closed {
            info!(mint = %pos.mint, pnl_sol = pnl, ?reason, "position closed");
            self.journal
                .write(
                    "position_close",
                    json!({ "mint": pos.mint, "reason": reason, "pnl_sol": pnl }),
                )
                .await;
            self.risk.record_exit(pnl, true).await;
        }
    }

    /// Drop a position we can no longer manage, valued at `price`.
    ///
    /// Zero for a genuine rug, the last observed price for one we simply cannot
    /// see any more. The second is a stale valuation and the journal records it
    /// as such, but the alternative is holding the slot forever.
    async fn force_close(&self, mint: &str, price: f64, reason: ExitReason) {
        let removed = { self.positions.lock().await.remove(mint) };
        if let Some(p) = removed {
            let pnl = p.pnl_sol(price);
            self.journal
                .write(
                    "position_close",
                    json!({ "mint": mint, "reason": reason, "pnl_sol": pnl }),
                )
                .await;
            self.risk.record_exit(pnl, true).await;
        }
    }
}

/// The exit rules, as a pure function.
///
/// Separated from execution deliberately: these are the rules that decide when
/// real money leaves a position, and they are worth testing without needing a
/// network, an executor, or a wall clock. `evaluate` above is then only
/// plumbing.
///
/// Returns the quantity to sell and why, or None to hold.
fn decide_exit(
    pos: &Position,
    price: f64,
    age_secs: i64,
    cfg: &ExitConfig,
) -> Option<(f64, ExitReason)> {
    let gain_bps = pos.unrealised_bps(price);

    // Stop loss first: it is the one rule whose entire job is to be fast.
    if gain_bps <= -cfg.stop_loss_bps {
        return Some((pos.tokens_held, ExitReason::StopLoss));
    }

    // Trailing stop. The peak must clear entry by at least the trail width
    // before this arms: peak is seeded at the entry price, so arming on "any
    // tick above entry" would let a trail tighter than the stop loss fire BELOW
    // entry and quietly override the configured stop.
    let arm_price = pos.entry_price_sol * (1.0 + cfg.trailing_stop_bps as f64 / 10_000.0);
    if pos.peak_price_sol >= arm_price
        && pos.drawdown_from_peak_bps(price) >= cfg.trailing_stop_bps
    {
        return Some((pos.tokens_held, ExitReason::TrailingStop));
    }

    if age_secs >= cfg.max_hold_secs {
        return Some((pos.tokens_held, ExitReason::MaxHold));
    }

    // Take-profit ladder: at most one rung per call, so a vertical candle does
    // not dump the whole position into its own spike.
    if let Some(rung) = cfg.take_profit.get(pos.next_rung) {
        if gain_bps >= rung.gain_bps {
            // Denominated in the ORIGINAL size, not what is left. A percentage
            // of the remainder means a ladder summing to 100% only ever sells
            // asymptotically and never fully exits.
            let qty = (pos.tokens_initial * (rung.pct / 100.0)).min(pos.tokens_held);
            if qty > 0.0 {
                return Some((qty, ExitReason::TakeProfit));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TakeProfitRung;

    fn cfg() -> ExitConfig {
        ExitConfig {
            take_profit: vec![
                TakeProfitRung { gain_bps: 5_000, pct: 50.0 },
                TakeProfitRung { gain_bps: 15_000, pct: 50.0 },
            ],
            stop_loss_bps: 3_500,
            trailing_stop_bps: 2_500,
            max_hold_secs: 900,
            poll_interval_ms: 2_000,
            dust_value_sol: 0.005,
        }
    }

    /// Entry at 1.0 SOL/token, 1000 tokens, nothing sold yet.
    fn pos() -> Position {
        Position {
            mint: "M".into(),
            venue: Venue::PumpFun,
            decimals: 6,
            opened_at: Utc::now(),
            entry_price_sol: 1.0,
            tokens_held: 1_000.0,
            tokens_initial: 1_000.0,
            sol_invested: 1_000.0,
            sol_realised: 0.0,
            peak_price_sol: 1.0,
            next_rung: 0,
            unroutable_strikes: 0,
            last_price_sol: None,
        }
    }

    use crate::config::{PaperConfig, RiskConfig};
    use crate::exec::paper::PaperExecutor;
    use crate::rpc::PriceSource;
    use std::collections::VecDeque;

    /// One mark outcome. `None` is a transport failure, `Some(None)` is "no
    /// route", `Some(Some(p))` is a real price - the three cases the manager
    /// must keep distinct.
    type Tick = Option<Option<f64>>;

    struct StubPrices {
        ticks: Mutex<VecDeque<Tick>>,
        last: Mutex<Tick>,
    }

    impl StubPrices {
        fn new(ticks: Vec<Tick>) -> Self {
            Self {
                ticks: Mutex::new(ticks.into()),
                last: Mutex::new(Some(Some(1.0))),
            }
        }
    }

    #[async_trait::async_trait]
    impl PriceSource for StubPrices {
        async fn mark(&self, _mint: &str, _held: f64, _dec: u8) -> Result<Option<f64>> {
            let tick = match self.ticks.lock().await.pop_front() {
                Some(t) => {
                    *self.last.lock().await = t;
                    t
                }
                // Past the end of the script, hold the last value.
                None => *self.last.lock().await,
            };
            match tick {
                None => Err(anyhow::anyhow!("simulated transport failure")),
                Some(v) => Ok(v),
            }
        }
    }

    fn temp_journal() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "solsnipe-test-{}-{}.jsonl",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Zero slippage and zero fees so the assertions below isolate position
    /// accounting from the paper cost model, which is tested separately.
    async fn harness(ticks: Vec<Tick>) -> (Arc<PositionManager>, Arc<RiskManager>) {
        harness_with(ticks, temp_journal().join("never").display().to_string()).await
    }

    async fn harness_with(
        ticks: Vec<Tick>,
        kill_switch_file: String,
    ) -> (Arc<PositionManager>, Arc<RiskManager>) {
        let risk = Arc::new(RiskManager::new(RiskConfig {
            position_size_sol: 1.0,
            max_concurrent_positions: 3,
            max_daily_loss_sol: 1_000.0,
            max_trades_per_day: 100,
            cooldown_ms: 0,
            kill_switch_file,
        }));
        let paper = Arc::new(PaperExecutor::new(PaperConfig {
            starting_balance_sol: 10_000.0,
            slippage_bps: 0,
            priority_fee_sol: 0.0,
            latency_penalty_bps: 0,
            fill_probability: 1.0,
        }));
        let journal = Arc::new(Journal::open(&temp_journal()).await.unwrap());
        let pm = Arc::new(PositionManager::new(
            Arc::new(StubPrices::new(ticks)),
            paper,
            cfg(),
            risk.clone(),
            journal,
        ));
        (pm, risk)
    }

    /// Entry at 1.0 SOL/token, 1000 tokens, 1000 SOL in.
    async fn opened(pm: &PositionManager, risk: &RiskManager) {
        assert!(risk.try_enter().await.allowed());
        pm.open("M".into(), Venue::PumpFun, 6, 1.0, 1_000.0, 1_000.0).await;
    }

    #[tokio::test]
    async fn a_winning_position_walks_the_ladder_and_closes_flat() {
        let (pm, risk) = harness(vec![Some(Some(1.6)), Some(Some(3.0))]).await;
        opened(&pm, &risk).await;

        pm.sweep().await.unwrap(); // +60%: first rung sells 500
        assert_eq!(pm.open_count().await, 1, "half the position remains");

        pm.sweep().await.unwrap(); // +200%: second rung sells the rest
        assert_eq!(pm.open_count().await, 0);

        let (_, pnl, open) = risk.snapshot().await;
        assert_eq!(open, 0, "closing a position must release the risk slot");
        // 500 @ 1.6 = 800, 500 @ 3.0 = 1500, less 1000 invested
        assert!((pnl - 1_300.0).abs() < 1e-6, "pnl was {pnl}");
    }

    #[tokio::test]
    async fn a_losing_position_stops_out_and_releases_the_slot() {
        let (pm, risk) = harness(vec![Some(Some(0.6))]).await;
        opened(&pm, &risk).await;

        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 0);

        let (_, pnl, open) = risk.snapshot().await;
        assert_eq!(open, 0);
        assert!((pnl + 400.0).abs() < 1e-6, "pnl was {pnl}");
    }

    // REGRESSION: mark failures once collapsed into "no route", so a single
    // aggregator outage force-closed every open position at a total loss.
    #[tokio::test]
    async fn a_transport_failure_never_closes_a_position() {
        let (pm, risk) = harness(vec![None, None, None, None, None]).await;
        opened(&pm, &risk).await;

        for _ in 0..5 {
            pm.sweep().await.unwrap();
        }
        assert_eq!(
            pm.open_count().await,
            1,
            "an outage is evidence about nothing and must not book a loss"
        );
    }

    // REGRESSION, measured live over a full hour: a Mark::Unknown used to skip
    // evaluate() entirely, so max_hold never ran and the position was held
    // forever. Ten positions sat open for 58 minutes against a 15 minute
    // window, and the mark budget they burned starved screening so badly that
    // approvals went to ZERO for the last 50 minutes of the run.
    //
    // Worth remembering that the adversarial review raised exactly this and the
    // verifier refuted it. The traffic settled the argument.
    #[tokio::test]
    async fn an_unmarkable_position_is_released_after_max_hold() {
        let (pm, risk) = harness(vec![None; 12]).await; // every mark fails
        opened(&pm, &risk).await;

        // Back-date it past the hold window, with one price previously seen.
        {
            let mut map = pm.positions.lock().await;
            let p = map.values_mut().next().unwrap();
            p.opened_at = Utc::now() - chrono::Duration::seconds(cfg().max_hold_secs + 1);
            p.last_price_sol = Some(0.9);
        }

        pm.sweep().await.unwrap();

        assert_eq!(
            pm.open_count().await,
            0,
            "a position we cannot manage must not be held indefinitely"
        );
        let (_, pnl, open) = risk.snapshot().await;
        assert_eq!(open, 0, "the risk slot has to come back");
        // Valued at the last price actually observed: 1000 * 0.9 - 1000 in.
        assert!((pnl + 100.0).abs() < 1e-6, "pnl was {pnl}");
    }

    #[tokio::test]
    async fn an_unmarkable_position_falls_back_to_entry_price_if_never_marked() {
        let (pm, risk) = harness(vec![None; 12]).await;
        opened(&pm, &risk).await;
        {
            let mut map = pm.positions.lock().await;
            let p = map.values_mut().next().unwrap();
            p.opened_at = Utc::now() - chrono::Duration::seconds(cfg().max_hold_secs + 1);
            // last_price_sol stays None: never successfully marked.
        }
        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 0);
        let (_, pnl, _) = risk.snapshot().await;
        assert!((pnl).abs() < 1e-6, "entry-priced close should be flat, got {pnl}");
    }

    #[tokio::test]
    async fn three_consecutive_no_route_sweeps_write_the_position_off() {
        let (pm, risk) = harness(vec![Some(None), Some(None), Some(None)]).await;
        opened(&pm, &risk).await;

        pm.sweep().await.unwrap();
        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 1, "two strikes must not be fatal");

        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 0);

        let (_, pnl, open) = risk.snapshot().await;
        assert_eq!(open, 0);
        assert!((pnl + 1_000.0).abs() < 1e-6, "a rug books the whole stake, got {pnl}");
    }

    #[tokio::test]
    async fn a_good_mark_resets_the_strike_count() {
        let (pm, risk) = harness(vec![
            Some(None),
            Some(None),
            Some(Some(1.1)), // routable again, and too small to trigger any exit
            Some(None),
            Some(None),
        ])
        .await;
        opened(&pm, &risk).await;

        for _ in 0..5 {
            pm.sweep().await.unwrap();
        }
        assert_eq!(
            pm.open_count().await,
            1,
            "strikes must not accumulate across a recovery"
        );
    }

    #[tokio::test]
    async fn the_kill_switch_flattens_open_positions() {
        let kill = temp_journal().with_extension("KILL");
        std::fs::write(&kill, b"stop").unwrap();

        let (pm, risk) = harness_with(vec![Some(Some(1.2))], kill.display().to_string()).await;
        pm.open("M".into(), Venue::PumpFun, 6, 1.0, 1_000.0, 1_000.0).await;

        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 0, "kill switch must flatten everything");

        let _ = std::fs::remove_file(&kill);
        let (_, pnl, _) = risk.snapshot().await;
        // Flattened at 1.2 with no fees: 1000 tokens * 1.2 = 1200, less 1000 in.
        assert!((pnl - 200.0).abs() < 1e-6, "pnl was {pnl}");
    }

    // The peak lives in the map, not in the clone `evaluate` works from. If it
    // failed to persist, a trailing stop could never fire on a later sweep.
    #[tokio::test]
    async fn the_peak_persists_across_sweeps() {
        let (pm, risk) = harness(vec![Some(Some(2.0)), Some(Some(1.4))]).await;
        opened(&pm, &risk).await;

        pm.sweep().await.unwrap(); // +100%: first rung sells half, peak -> 2.0
        assert_eq!(pm.open_count().await, 1);

        // 1.4 is a 30% drawdown from the remembered 2.0 peak, past the 25%
        // trail. Against a peak that reset to the entry price it would be a
        // 40% GAIN and nothing would fire.
        pm.sweep().await.unwrap();
        assert_eq!(
            pm.open_count().await,
            0,
            "the trailing stop must see the peak from the previous sweep"
        );
    }

    #[tokio::test]
    async fn a_residual_below_the_dust_threshold_closes_and_books_at_zero() {
        // 0.006 tokens at 1.0. The first rung sells half at 1.6, leaving
        // 0.003 tokens worth 0.0048 SOL - under the 0.005 dust threshold.
        let (pm, risk) = harness(vec![Some(Some(1.6))]).await;
        assert!(risk.try_enter().await.allowed());
        pm.open("M".into(), Venue::PumpFun, 6, 1.0, 0.006, 0.006).await;

        pm.sweep().await.unwrap();
        assert_eq!(pm.open_count().await, 0, "dust must not be chased");

        let (_, pnl, open) = risk.snapshot().await;
        assert_eq!(open, 0);
        // Realised 0.0048, invested 0.006, residual abandoned at ZERO rather
        // than at the 1.6 mark we declined to trade at.
        assert!((pnl + 0.0012).abs() < 1e-9, "pnl was {pnl}");
    }

    #[tokio::test]
    async fn the_concurrency_cap_is_enforced_and_released() {
        let (_pm, risk) = harness(vec![]).await;
        for _ in 0..3 {
            assert!(risk.try_enter().await.allowed());
        }
        assert!(
            !risk.try_enter().await.allowed(),
            "a 4th entry must be blocked at max_concurrent_positions = 3"
        );
        risk.release_entry().await;
        assert!(
            risk.try_enter().await.allowed(),
            "releasing an unfilled reservation must free the slot"
        );
    }

    #[test]
    fn holds_when_nothing_triggers() {
        assert!(decide_exit(&pos(), 1.10, 0, &cfg()).is_none());
    }

    #[test]
    fn stop_loss_sells_the_whole_position() {
        let (qty, reason) = decide_exit(&pos(), 0.60, 0, &cfg()).unwrap();
        assert_eq!(reason, ExitReason::StopLoss);
        assert_eq!(qty, 1_000.0);
    }

    #[test]
    fn max_hold_fires_on_age_alone() {
        let (qty, reason) = decide_exit(&pos(), 1.05, 900, &cfg()).unwrap();
        assert_eq!(reason, ExitReason::MaxHold);
        assert_eq!(qty, 1_000.0);
    }

    // REGRESSION: rungs were once a share of what REMAINED, so a ladder summing
    // to 100% only ever sold asymptotically and never fully exited.
    #[test]
    fn take_profit_rung_is_a_share_of_the_original_size() {
        let mut p = pos();
        p.next_rung = 1;
        p.tokens_held = 500.0; // first rung already sold half
        let (qty, reason) = decide_exit(&p, 3.0, 0, &cfg()).unwrap();
        assert_eq!(reason, ExitReason::TakeProfit);
        assert_eq!(qty, 500.0, "a ladder summing to 100% must fully exit");
    }

    #[test]
    fn take_profit_never_sells_more_than_is_held() {
        let mut p = pos();
        p.tokens_held = 100.0; // less than the rung's 50% of the original
        let (qty, _) = decide_exit(&p, 2.0, 0, &cfg()).unwrap();
        assert_eq!(qty, 100.0);
    }

    #[test]
    fn ladder_is_exhausted_after_the_last_rung() {
        let mut p = pos();
        p.next_rung = 2;
        assert!(decide_exit(&p, 100.0, 0, &cfg()).is_none());
    }

    // REGRESSION: the trail once armed on ANY tick above entry. With a 25% trail
    // and a 35% stop, that fired at 0.7575 - below entry, and tighter than the
    // stop the operator actually configured.
    #[test]
    fn trailing_stop_does_not_arm_just_above_entry() {
        let mut p = pos();
        p.peak_price_sol = 1.01;
        // 0.70 is chosen so the drawdown from that 1.01 peak is ~3069 bps -
        // comfortably past the 2500 trail - while still inside the 3500 stop.
        // With the arming bug this fires TrailingStop at 0.70: below entry, and
        // tighter than the stop the operator configured. A gentler price would
        // pass either way and prove nothing.
        assert!(
            decide_exit(&p, 0.70, 0, &cfg()).is_none(),
            "a 1% peak must not arm a 25% trail"
        );
    }

    #[test]
    fn trailing_stop_arms_once_the_peak_clears_the_trail_width() {
        let mut p = pos();
        p.peak_price_sol = 1.30; // clears the 1.25 arm threshold
        let (qty, reason) = decide_exit(&p, 0.95, 0, &cfg()).unwrap();
        assert_eq!(reason, ExitReason::TrailingStop);
        assert_eq!(qty, 1_000.0);
    }

    #[test]
    fn stop_loss_takes_precedence_over_the_trailing_stop() {
        let mut p = pos();
        p.peak_price_sol = 2.00;
        // Down 40%: past the 35% stop, and also a big drawdown from peak.
        let (_, reason) = decide_exit(&p, 0.60, 0, &cfg()).unwrap();
        assert_eq!(reason, ExitReason::StopLoss);
    }
}
