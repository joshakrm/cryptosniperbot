use chrono::{DateTime, Datelike, Utc};
use std::path::PathBuf;
use tokio::sync::Mutex;

use crate::config::RiskConfig;

#[derive(Debug, Clone)]
pub enum RiskDecision {
    Allow,
    Block(String),
}

impl RiskDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, RiskDecision::Allow)
    }
}

#[derive(Debug)]
struct RiskState {
    day: u32,
    trades_today: u64,
    realised_pnl_today: f64,
    open_positions: usize,
    last_entry: Option<DateTime<Utc>>,
    /// Restored by `release_entry` so a reservation that never became a trade
    /// does not burn a cooldown window. Without this, losing a simulated race
    /// blocks the NEXT candidate, and that candidate is then discarded
    /// permanently because main.rs has already recorded its mint as seen.
    prev_last_entry: Option<DateTime<Utc>>,
}

/// Enforces the limits that stop one bad night from becoming the whole account.
///
/// Every gate here is checked immediately before an entry, never cached. The
/// kill switch is a file on disk rather than a signal handler on purpose: you
/// can stop the bot from any terminal, any script, or a phone over SSH, without
/// needing to find the process.
pub struct RiskManager {
    cfg: RiskConfig,
    kill_file: PathBuf,
    state: Mutex<RiskState>,
}

impl RiskManager {
    pub fn new(cfg: RiskConfig) -> Self {
        let kill_file = PathBuf::from(&cfg.kill_switch_file);
        Self {
            cfg,
            kill_file,
            state: Mutex::new(RiskState {
                day: Utc::now().ordinal(),
                trades_today: 0,
                realised_pnl_today: 0.0,
                open_positions: 0,
                last_entry: None,
                prev_last_entry: None,
            }),
        }
    }

    pub fn kill_switch_engaged(&self) -> bool {
        self.kill_file.exists()
    }

    /// Check every gate AND reserve the slot inside one critical section.
    ///
    /// Candidates are screened in concurrently spawned tasks. A gate that
    /// returns Allow and leaves the caller to increment later is a
    /// check-then-act race: several candidates all clear the concurrency limit
    /// before any of them takes a slot, and the bot quietly runs over
    /// max_concurrent_positions and max_trades_per_day. Reserving here closes
    /// that window. Callers that do not go on to enter MUST call
    /// `release_entry` to hand the slot back.
    pub async fn try_enter(&self) -> RiskDecision {
        if self.kill_switch_engaged() {
            return RiskDecision::Block(format!(
                "kill switch file {} is present",
                self.kill_file.display()
            ));
        }

        let mut st = self.state.lock().await;
        self.roll_day(&mut st);

        if st.open_positions >= self.cfg.max_concurrent_positions {
            return RiskDecision::Block(format!(
                "at max concurrent positions ({})",
                self.cfg.max_concurrent_positions
            ));
        }
        if st.trades_today >= self.cfg.max_trades_per_day {
            return RiskDecision::Block(format!(
                "daily trade cap reached ({})",
                self.cfg.max_trades_per_day
            ));
        }
        if st.realised_pnl_today <= -self.cfg.max_daily_loss_sol {
            return RiskDecision::Block(format!(
                "daily loss cap hit ({:.3} SOL down, limit {:.3})",
                st.realised_pnl_today, self.cfg.max_daily_loss_sol
            ));
        }
        if let Some(last) = st.last_entry {
            let since = (Utc::now() - last).num_milliseconds();
            if since < self.cfg.cooldown_ms as i64 {
                return RiskDecision::Block(format!(
                    "cooldown: {}ms since last entry, need {}ms",
                    since, self.cfg.cooldown_ms
                ));
            }
        }

        // Reserved under the same lock that just approved it.
        st.trades_today += 1;
        st.open_positions += 1;
        st.prev_last_entry = st.last_entry;
        st.last_entry = Some(Utc::now());

        RiskDecision::Allow
    }

    /// Hand back a slot reserved by `try_enter` when the entry did not happen.
    pub async fn release_entry(&self) {
        let mut st = self.state.lock().await;
        st.trades_today = st.trades_today.saturating_sub(1);
        st.open_positions = st.open_positions.saturating_sub(1);
        // Hand the cooldown back too: no trade happened, so nothing should be
        // cooling down. Exact for one outstanding reservation, which the
        // cooldown itself normally enforces.
        st.last_entry = st.prev_last_entry;
    }

    pub async fn record_exit(&self, realised_pnl_sol: f64, position_closed: bool) {
        let mut st = self.state.lock().await;
        self.roll_day(&mut st);
        st.realised_pnl_today += realised_pnl_sol;
        if position_closed {
            st.open_positions = st.open_positions.saturating_sub(1);
        }
    }

    pub async fn snapshot(&self) -> (u64, f64, usize) {
        let st = self.state.lock().await;
        (st.trades_today, st.realised_pnl_today, st.open_positions)
    }

    /// Reset daily counters when the UTC day rolls over. Open position count is
    /// deliberately NOT reset, since those positions are still open.
    fn roll_day(&self, st: &mut RiskState) {
        let today = Utc::now().ordinal();
        if st.day != today {
            st.day = today;
            st.trades_today = 0;
            st.realised_pnl_today = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cooldown_ms: u64) -> RiskConfig {
        RiskConfig {
            position_size_sol: 1.0,
            max_concurrent_positions: 3,
            max_daily_loss_sol: 100.0,
            max_trades_per_day: 100,
            cooldown_ms,
            kill_switch_file: "/nonexistent/solsnipe-KILL".to_string(),
        }
    }

    // REGRESSION: release_entry restored the counters but not last_entry, so
    // losing a race burned a full cooldown window. The candidate blocked by
    // that phantom cooldown was then lost for good, because main.rs had already
    // recorded its mint in the `seen` set.
    #[tokio::test]
    async fn releasing_a_reservation_hands_back_the_cooldown() {
        let risk = RiskManager::new(cfg(60_000));
        assert!(risk.try_enter().await.allowed());
        risk.release_entry().await;
        assert!(
            risk.try_enter().await.allowed(),
            "an entry that never happened must not start a cooldown"
        );
    }

    #[tokio::test]
    async fn a_completed_entry_still_enforces_the_cooldown() {
        let risk = RiskManager::new(cfg(60_000));
        assert!(risk.try_enter().await.allowed());
        assert!(
            !risk.try_enter().await.allowed(),
            "back-to-back entries must still be rate limited"
        );
    }

    #[tokio::test]
    async fn the_daily_loss_cap_blocks_further_entries() {
        let risk = RiskManager::new(cfg(0));
        risk.record_exit(-100.0, true).await;
        assert!(!risk.try_enter().await.allowed());
    }
}
