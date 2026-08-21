use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcConfig,
    pub programs: ProgramConfig,
    pub screen: ScreenConfig,
    pub risk: RiskConfig,
    pub exit: ExitConfig,
    pub paper: PaperConfig,
    #[serde(default)]
    pub live: LiveConfig,
    #[serde(default)]
    pub shadow: ShadowConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
    pub jupiter_url: String,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "d_commitment")]
    pub commitment: String,
    /// Minimum gap between outbound Jupiter requests, shared across screening
    /// and position marks. The free aggregator tier throttles around 1/sec and
    /// a 429 is indistinguishable from useful information, so it is better to
    /// queue than to fire and fail. 0 disables the limiter.
    #[serde(default = "d_jup_interval")]
    pub jupiter_min_interval_ms: u64,
    /// Slowest the adaptive throttle will back off to before giving up on a
    /// request. Bounds how bad a throttling episode can get.
    #[serde(default = "d_jup_max_interval")]
    pub jupiter_max_interval_ms: u64,
    /// How many times wider a position mark's gap is than a screening quote's.
    /// Marks are frequent and merely useful; screening is rare and decisive.
    #[serde(default = "d_jup_mark_mult")]
    pub jupiter_mark_multiplier: u32,
}
fn d_jup_max_interval() -> u64 { 4000 }
fn d_jup_mark_mult() -> u32 { 3 }
fn d_jup_interval() -> u64 { 250 }
fn d_true() -> bool { true }
fn d_timeout() -> u64 { 4000 }
fn d_commitment() -> String { "confirmed".to_string() }

#[derive(Debug, Clone, Deserialize)]
pub struct ProgramConfig {
    pub pump_fun: String,
    pub pump_swap: String,
    pub raydium_amm_v4: String,
    pub raydium_cpmm: String,
    pub wsol_mint: String,
    pub usdc_mint: String,
    pub watch: Vec<String>,
}

impl ProgramConfig {
    /// Resolve the `watch` names into (label, program_id) pairs.
    pub fn watched(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for name in &self.watch {
            let id = match name.as_str() {
                "pump_fun" => &self.pump_fun,
                "pump_swap" => &self.pump_swap,
                "raydium_amm_v4" => &self.raydium_amm_v4,
                "raydium_cpmm" => &self.raydium_cpmm,
                other => bail!("unknown venue in programs.watch: {other}"),
            };
            out.push((name.clone(), id.clone()));
        }
        if out.is_empty() {
            bail!("programs.watch is empty - nothing to listen to");
        }
        Ok(out)
    }

    /// Mints we treat as the quote side, never as the token being launched.
    pub fn quote_mints(&self) -> Vec<&str> {
        vec![self.wsol_mint.as_str(), self.usdc_mint.as_str()]
    }
}

/// What to do when a check cannot run at all, as opposed to running and
/// failing. Rejecting is the default: an unanswered question about safety is
/// not a safe answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailablePolicy {
    Reject,
    Advisory,
}
fn d_unavailable() -> UnavailablePolicy { UnavailablePolicy::Reject }

#[derive(Debug, Clone, Deserialize)]
pub struct ScreenConfig {
    pub min_liquidity_sol: f64,
    pub max_top10_pct: f64,
    pub min_holders: u64,
    pub require_mint_authority_renounced: bool,
    pub require_freeze_authority_renounced: bool,
    pub reject_token2022_extensions: bool,
    /// Reject pools whose LP tokens are still outstanding, i.e. whose liquidity
    /// the creator can withdraw. Bonding-curve launches have no LP mint at all
    /// and always pass. Rejects locker-held LP too, which is the fail-closed
    /// direction - see screen/liquidity.rs.
    #[serde(default = "d_true")]
    pub require_lp_burned: bool,
    /// Run the holder-distribution checks at all.
    ///
    /// They need the token-account index, which lags a launch by ~13s, so they
    /// force a `min_pool_age_ms` wait to be usable. Turning them off is what
    /// makes a fast-entry configuration possible - and it is a real reduction
    /// in safety, not a free win: nothing else in the gauntlet looks at how
    /// supply is distributed.
    #[serde(default = "d_true")]
    pub require_holders: bool,
    /// Minimum SOL placed in the pool at launch. 0 disables the check.
    ///
    /// This is the only filter that costs nothing: the figure comes out of the
    /// launch transaction, which is fetched anyway to find the mint. That
    /// matters because the aggregator budget, not the RPC, is the binding
    /// constraint - the holder checks were quietly rejecting 79% of candidates
    /// before they reached three quotes, and nothing else at t=0 came close.
    ///
    /// Measured over 225 live launches: median pool is 0.58 SOL, and a 2 SOL
    /// floor rejects 62%. It is also economically sensible rather than
    /// arbitrary - a pool this thin fails the depth check later anyway, after
    /// spending the quotes this check exists to save.
    #[serde(default)]
    pub min_pool_sol: f64,
    pub max_roundtrip_loss_bps: u64,
    pub max_screen_ms: u64,
    /// Trading through an endpoint that keeps failing means trading blind. Only
    /// set this to "advisory" deliberately, and expect worse selection.
    #[serde(default = "d_unavailable")]
    pub treat_unavailable_as: UnavailablePolicy,
    #[serde(default)]
    pub min_pool_age_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub position_size_sol: f64,
    pub max_concurrent_positions: usize,
    pub max_daily_loss_sol: f64,
    pub max_trades_per_day: u64,
    pub cooldown_ms: u64,
    pub kill_switch_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TakeProfitRung {
    pub gain_bps: i64,
    pub pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExitConfig {
    pub take_profit: Vec<TakeProfitRung>,
    pub stop_loss_bps: i64,
    pub trailing_stop_bps: i64,
    pub max_hold_secs: i64,
    pub poll_interval_ms: u64,
    /// Residual position value below which we stop bothering to sell. Denominated
    /// in SOL, not token count: after a big run-up, 0.1% of the tokens can still
    /// be a meaningful fraction of the stake.
    #[serde(default = "d_dust")]
    pub dust_value_sol: f64,
}
fn d_dust() -> f64 { 0.005 }

#[derive(Debug, Clone, Deserialize)]
pub struct PaperConfig {
    pub starting_balance_sol: f64,
    pub slippage_bps: u64,
    pub priority_fee_sol: f64,
    pub latency_penalty_bps: u64,
    pub fill_probability: f64,
}

/// Follows a sample of candidates the bot did NOT trade, so filters can be
/// judged on outcomes rather than on plausibility. See src/shadow.rs.
#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Percent of candidates to follow. Kept small: this is a control group,
    /// not a second strategy, and every shadow costs quotes.
    #[serde(default = "d_shadow_pct")]
    pub sample_pct: f64,
    /// Fixed notional used for every shadow price, so returns are comparable
    /// across candidates instead of varying with an intended position size.
    #[serde(default = "d_shadow_probe")]
    pub probe_size_sol: f64,
    /// Seconds after first sight at which to price the token.
    #[serde(default = "d_shadow_marks")]
    pub marks_secs: Vec<u64>,
}
fn d_shadow_pct() -> f64 { 5.0 }
fn d_shadow_probe() -> f64 { 0.1 }
fn d_shadow_marks() -> Vec<u64> { vec![60, 300, 900] }

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_pct: d_shadow_pct(),
            probe_size_sol: d_shadow_probe(),
            marks_secs: d_shadow_marks(),
        }
    }
}

#[allow(dead_code)] // parsed to keep the config shape stable; live exec is unimplemented
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub keypair_path: String,
    #[serde(default)]
    pub jito_tip_sol: f64,
    #[serde(default)]
    pub max_slippage_bps: u64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("cannot parse config at {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.live.enabled {
            bail!(
                "live.enabled = true, but live execution is not implemented in this build. \
                 Refusing to start rather than pretending to trade. Set live.enabled = false."
            );
        }
        if self.rpc.http_url.contains("YOUR_KEY") || self.rpc.ws_url.contains("YOUR_KEY") {
            bail!("rpc urls still contain the YOUR_KEY placeholder - set real endpoints");
        }
        if self.risk.position_size_sol <= 0.0 {
            bail!("risk.position_size_sol must be > 0");
        }
        if !(0.0..=1.0).contains(&self.paper.fill_probability) {
            bail!("paper.fill_probability must be between 0.0 and 1.0");
        }
        let tp_total: f64 = self.exit.take_profit.iter().map(|r| r.pct).sum();
        if tp_total <= 0.0 {
            bail!("exit.take_profit has no rungs - positions would never take profit");
        }
        // Rungs are denominated in the ORIGINAL position size, so anything over
        // 100% is trying to sell tokens that were never held.
        if tp_total > 100.0 {
            bail!("exit.take_profit rungs sum to {tp_total}% of the original position, max 100");
        }
        // getTokenLargestAccounts returns at most 20 entries, so a higher
        // threshold can never be satisfied and would reject every candidate
        // forever without ever saying why.
        if self.screen.min_holders > 20 {
            bail!(
                "screen.min_holders = {} is unsatisfiable: getTokenLargestAccounts returns at most 20",
                self.screen.min_holders
            );
        }
        // Screening makes up to three Jupiter quotes, and the adaptive throttle
        // may space them as widely as jupiter_max_interval_ms. If the screen
        // budget is narrower than that, screening times out on exactly the
        // candidates that got far enough to matter - and it fails as
        // `screen_timeout`, which looks like a slow endpoint rather than a
        // misconfiguration. Measured: an 8s budget against a 4s ceiling killed
        // 40-60 candidates per five minutes while every number looked healthy.
        const QUOTES_PER_SCREEN: u64 = 3;
        let worst_case = QUOTES_PER_SCREEN * self.rpc.jupiter_max_interval_ms;
        if self.screen.max_screen_ms < worst_case {
            bail!(
                "screen.max_screen_ms ({}) is below the worst case for {} Jupiter quotes at rpc.jupiter_max_interval_ms ({}ms each = {}ms). Screening would time out whenever the throttle backs off. Raise max_screen_ms to at least {}, or lower jupiter_max_interval_ms.",
                self.screen.max_screen_ms,
                QUOTES_PER_SCREEN,
                self.rpc.jupiter_max_interval_ms,
                worst_case,
                worst_case
            );
        }

        if self.exit.dust_value_sol < 0.0 {
            bail!("exit.dust_value_sol must be >= 0");
        }
        // tokio::time::interval panics on a zero period. That panic kills only
        // the sweeper task, leaving a process that can still open positions but
        // can never close one - the worst possible failure mode here.
        if self.exit.poll_interval_ms == 0 {
            bail!("exit.poll_interval_ms must be > 0 or positions would never be swept");
        }
        if self.exit.max_hold_secs <= 0 {
            bail!("exit.max_hold_secs must be > 0");
        }
        self.programs.watched()?;
        Ok(())
    }
}
