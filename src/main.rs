mod config;
mod decode;
mod exec;
mod ingest;
mod journal;
mod position;
mod risk;
mod rpc;
mod screen;
mod shadow;
mod types;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::exec::paper::PaperExecutor;
use crate::exec::Executor;
use crate::journal::Journal;
use crate::position::PositionManager;
use crate::risk::RiskManager;
use crate::rpc::{Jupiter, JupiterPrices, PriceSource, SolanaRpc};
use crate::screen::{LaunchContext, Screener};
use crate::types::Venue;

#[derive(Parser)]
#[command(
    name = "solsnipe",
    version,
    about = "Solana new-pool sniper. Fail-closed screening, paper execution by default."
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,

    #[arg(long, default_value = "journal.jsonl", global = true)]
    journal: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch for launches and trade them on paper.
    Run,
    /// Screen a single mint and print the verdict. Use this to sanity-check
    /// your thresholds against tokens whose outcome you already know.
    Screen {
        mint: String,
        /// Venue to screen as: pump_fun, pump_swap, raydium_amm_v4, raydium_cpmm.
        /// Matters because concentration is advisory on a pump.fun bonding
        /// curve and fatal everywhere else - screening as "unknown" would
        /// disagree with the verdict the live path reaches.
        #[arg(long, default_value = "unknown")]
        venue: String,
    },
    /// Summarise a journal file: fills, PnL, and why candidates were rejected.
    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Run => run(&cli.config, &cli.journal).await,
        Cmd::Screen { ref mint, ref venue } => screen_one(&cli.config, mint, venue).await,
        Cmd::Stats => stats(&cli.journal).await,
    }
}

async fn screen_one(config_path: &Path, mint: &str, venue: &str) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let rpc = SolanaRpc::new(&cfg.rpc)?;
    let jup = Jupiter::new(&cfg.rpc)?;
    let screener = Screener::new(
        rpc,
        jup,
        cfg.screen.clone(),
        cfg.programs.wsol_mint.clone(),
        cfg.risk.position_size_sol,
    );

    // No transaction here, so the pool account is unknown and concentration
    // measures every holder rather than guessing which one is the pool.
    // No transaction here, so neither the pool account nor the LP mint is
    // known: concentration measures every holder and the LP check has nothing
    // to inspect.
    let report = screener
        .screen(&mint.to_string(), Venue::from_label(venue), &LaunchContext::default())
        .await;

    println!("\nmint: {mint}");
    println!("{}\n", report.summary());
    for c in &report.checks {
        let mark = match (c.passed, c.severity) {
            (true, _) => "PASS",
            (false, crate::types::Severity::Advisory) => "WARN",
            (false, crate::types::Severity::Unavailable) => "UNCH",
            (false, _) => "FAIL",
        };
        println!("  [{mark}] {:<22} {}", c.name, c.detail);
    }
    if let Some(p) = report.quoted_price_sol {
        println!("\n  quoted entry: {p:.12} SOL per token");
    }
    println!();
    Ok(())
}

async fn run(config_path: &Path, journal_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let journal = Arc::new(Journal::open(journal_path).await?);
    let rpc = SolanaRpc::new(&cfg.rpc)?;
    let jup = Jupiter::new(&cfg.rpc)?;

    let screener = Arc::new(Screener::new(
        rpc.clone(),
        jup.clone(),
        cfg.screen.clone(),
        cfg.programs.wsol_mint.clone(),
        cfg.risk.position_size_sol,
    ));
    let risk = Arc::new(RiskManager::new(cfg.risk.clone()));

    let paper = Arc::new(PaperExecutor::new(cfg.paper.clone()));
    let executor: Arc<dyn Executor> = paper.clone();

    let prices: Arc<dyn PriceSource> = Arc::new(JupiterPrices::new(
        jup.clone(),
        cfg.programs.wsol_mint.clone(),
    ));
    let shadow = Arc::new(crate::shadow::Shadow::new(
        prices.clone(),
        journal.clone(),
        cfg.shadow.clone(),
    ));
    if shadow.enabled() {
        info!(
            sample_pct = cfg.shadow.sample_pct,
            "shadow tracking on - following a sample of candidates the bot does not trade"
        );
    }

    let pm = Arc::new(PositionManager::new(
        prices,
        executor.clone(),
        cfg.exit.clone(),
        risk.clone(),
        journal.clone(),
        shadow.clone(),
    ));

    info!(
        mode = executor.name(),
        balance = cfg.paper.starting_balance_sol,
        size = cfg.risk.position_size_sol,
        journal = %journal.path().display(),
        "solsnipe starting"
    );
    if risk.kill_switch_engaged() {
        warn!(
            file = %cfg.risk.kill_switch_file,
            "kill switch file exists - no entries will be taken until it is removed"
        );
    }

    tokio::spawn(pm.clone().run());

    // Heartbeat: without it a quiet night is indistinguishable from a hung
    // socket, and you find out at 3am that you stopped seeing launches.
    {
        let paper = paper.clone();
        let risk = risk.clone();
        let pm = pm.clone();
        let jup = jup.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let (balance, attempted, landed) = paper.stats().await;
                let (trades, pnl, _) = risk.snapshot().await;
                // Every await must be resolved BEFORE the macro call. tracing
                // builds non-Send `Arguments<'_>` temporaries that would then
                // straddle the await point and make this whole spawned future
                // non-Send, which tokio::spawn rejects.
                let open = pm.open_count().await;
                // The throttle discovers its own rate, so surface it: a value
                // pinned at the ceiling means the endpoint is the bottleneck.
                let jup_ms = jup.current_interval().await.as_millis() as u64;
                info!(
                    balance_sol = balance,
                    open,
                    trades_today = trades,
                    pnl_today = pnl,
                    races = attempted,
                    won = landed,
                    jup_interval_ms = jup_ms,
                    "heartbeat"
                );
            }
        });
    }

    let (tx, mut rx) = mpsc::channel(2048);
    tokio::spawn(ingest::run(cfg.clone(), tx));

    // Mints we have already made a decision about, so a token that emits
    // several matching logs is not screened (or bought) more than once.
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    while let Some(hit) = rx.recv().await {
        if !decode::is_launch_log(hit.venue, &hit.program_id, &hit.logs) {
            continue;
        }
        debug!(venue = ?hit.venue, sig = %hit.signature, "launch log matched");

        let ctx = CandidateCtx {
            cfg: cfg.clone(),
            rpc: rpc.clone(),
            screener: screener.clone(),
            risk: risk.clone(),
            executor: executor.clone(),
            pm: pm.clone(),
            shadow: shadow.clone(),
            journal: journal.clone(),
            seen: seen.clone(),
        };

        // Screening costs several round trips. Doing it inline would stall the
        // websocket consumer and we would miss every launch that overlaps it.
        tokio::spawn(async move {
            if let Err(e) = handle_candidate(ctx, hit).await {
                warn!(error = %e, "candidate handling failed");
            }
        });
    }

    Ok(())
}

#[derive(Clone)]
struct CandidateCtx {
    cfg: Config,
    rpc: SolanaRpc,
    screener: Arc<Screener>,
    risk: Arc<RiskManager>,
    executor: Arc<dyn Executor>,
    pm: Arc<PositionManager>,
    shadow: Arc<crate::shadow::Shadow>,
    journal: Arc<Journal>,
    seen: Arc<Mutex<HashSet<String>>>,
}

async fn handle_candidate(ctx: CandidateCtx, hit: ingest::LogHit) -> Result<()> {
    // The transaction is often not queryable the instant its log arrives.
    let tx = match fetch_tx_with_retry(&ctx.rpc, &hit.signature).await {
        Some(t) => t,
        None => {
            debug!(sig = %hit.signature, "transaction never became available");
            return Ok(());
        }
    };

    let quote_mints = ctx.cfg.programs.quote_mints();
    let mint = match decode::extract_mint(&tx, &quote_mints) {
        Some(m) => m,
        None => {
            debug!(sig = %hit.signature, "no single non-quote mint - skipping");
            return Ok(());
        }
    };

    // First decision wins; every later log for the same mint is dropped.
    {
        let mut seen = ctx.seen.lock().await;
        if !seen.insert(mint.clone()) {
            return Ok(());
        }
    }

    let creator = decode::extract_creator(&tx);
    let decimals = match resolve_decimals(&ctx.rpc, &tx, &mint).await {
        Some(d) => d,
        None => {
            debug!(%mint, "could not determine decimals - skipping");
            return Ok(());
        }
    };

    // Free metrics: the transaction is already in hand. Recorded, not enforced -
    // they exist so a threshold can later be chosen from a real distribution.
    let (creator_share_pct, pool_sol) = decode::extract_launch_metrics(&tx, &mint);

    ctx.journal
        .write(
            "candidate",
            json!({
                "mint": mint,
                "venue": hit.venue,
                "signature": hit.signature,
                "slot": hit.slot,
                "creator": creator,
                "decimals": decimals,
                "creator_share_pct": creator_share_pct,
                "pool_sol": pool_sol,
            }),
        )
        .await;

    // Optional deliberate delay: sniping the very first block is a losing game
    // against faster bots, and waiting lets the obvious rugs reveal themselves.
    if ctx.cfg.screen.min_pool_age_ms > 0 {
        tokio::time::sleep(Duration::from_millis(ctx.cfg.screen.min_pool_age_ms)).await;
    }

    let launch = LaunchContext {
        vault: decode::extract_vault_account(&tx, &mint),
        lp_mint: decode::extract_lp_mint(&tx, &mint, &quote_mints),
        other_mints: decode::extract_other_mints(&tx, &mint, &quote_mints),
        pool_sol,
    };
    let report = ctx.screener.screen(&mint, hit.venue, &launch).await;
    ctx.journal.write_typed("screen", &report).await;

    // Follow a sample of candidates regardless of the verdict. This has to
    // happen for REJECTED ones above all - they are the control group, and
    // without them there is no way to tell a filter that removes losers from
    // one that removes winners.
    ctx.shadow.clone().track(
        mint.clone(),
        hit.venue,
        decimals,
        crate::shadow::ShadowVerdict {
            approved: report.approved(),
            rejected_by: report
                .rejections()
                .iter()
                .map(|c| c.name.to_string())
                .collect(),
            pool_sol: launch.pool_sol,
            creator_share_pct,
        },
    );

    if !report.approved() {
        info!(%mint, venue = ?hit.venue, "{}", report.summary());
        return Ok(());
    }

    if ctx.pm.is_open(&mint).await {
        return Ok(());
    }

    let entry_price = match report.quoted_price_sol {
        Some(p) if p > 0.0 => p,
        _ => return Ok(()),
    };

    // Risk gates are checked after screening, not before, so the journal still
    // records what a blocked-but-good candidate would have been. This both
    // checks and reserves, so it sits as close to the buy as possible and every
    // path that does not fill must release.
    let decision = ctx.risk.try_enter().await;
    if !decision.allowed() {
        if let crate::risk::RiskDecision::Block(reason) = &decision {
            info!(%mint, %reason, "approved but blocked by risk");
            ctx.journal
                .write("risk_block", json!({ "mint": mint, "reason": reason }))
                .await;
        }
        return Ok(());
    }

    let size = ctx.cfg.risk.position_size_sol;
    let bought = match ctx.executor.buy(&mint, size, entry_price).await {
        Ok(b) => b,
        Err(e) => {
            // Never let an execution error strand a reserved slot.
            ctx.risk.release_entry().await;
            return Err(e);
        }
    };

    match bought {
        Some(fill) => {
            ctx.journal.write_typed("fill_buy", &fill).await;
            ctx.pm
                .open(
                    mint.clone(),
                    hit.venue,
                    decimals,
                    fill.price_sol,
                    fill.token_amount,
                    fill.sol_amount + fill.fees_sol,
                )
                .await;

            let (trades, pnl, open) = ctx.risk.snapshot().await;
            // Resolved before the macro: see the heartbeat above for why an
            // .await inside a tracing macro breaks Send.
            let balance = ctx.executor.balance_sol().await;
            info!(
                %mint,
                venue = ?hit.venue,
                price = fill.price_sol,
                sol = fill.sol_amount,
                open,
                trades_today = trades,
                pnl_today = pnl,
                balance,
                "ENTERED"
            );
        }
        None => {
            ctx.risk.release_entry().await;
            ctx.journal
                .write("missed", json!({ "mint": mint, "reason": "no fill" }))
                .await;
        }
    }

    Ok(())
}

async fn fetch_tx_with_retry(rpc: &SolanaRpc, sig: &str) -> Option<serde_json::Value> {
    // A transaction is frequently not queryable the instant its log arrives,
    // and a throttled endpoint makes that far worse: a live run on the public
    // RPC lost 51% of detections here. A paid endpoint needs far fewer attempts,
    // but the retries cost nothing when the first one succeeds.
    const ATTEMPTS: u32 = 6;
    let mut last: Option<String> = None;

    for attempt in 0..ATTEMPTS {
        match rpc.get_transaction(sig).await {
            Ok(v) if !v.is_null() => return Some(v),
            Ok(_) => last = Some("not visible yet".to_string()),
            Err(e) => last = Some(e.to_string()),
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
        }
    }

    // Logged here rather than at the call site so the REASON survives: a
    // throttled endpoint and a genuinely missing transaction need different fixes.
    debug!(
        signature = %sig,
        attempts = ATTEMPTS,
        reason = %last.unwrap_or_else(|| "unknown".to_string()),
        "gave up fetching transaction"
    );
    None
}

async fn resolve_decimals(
    rpc: &SolanaRpc,
    tx: &serde_json::Value,
    mint: &str,
) -> Option<u8> {
    if let Some(d) = decode::extract_decimals(tx, mint) {
        return Some(d);
    }
    rpc.get_token_supply(&mint.to_string())
        .await
        .ok()?
        .get("value")?
        .get("decimals")?
        .as_u64()
        .map(|d| d as u8)
}

async fn stats(journal_path: &Path) -> Result<()> {
    use std::collections::HashMap;

    let raw = std::fs::read_to_string(journal_path)
        .with_context(|| format!("cannot read journal at {}", journal_path.display()))?;

    let mut candidates = 0u64;
    let mut approved = 0u64;
    let mut buys = 0u64;
    let mut closes = 0u64;
    let mut wins = 0u64;
    let mut pnl = 0.0f64;
    let mut misses = 0u64;
    let mut reject_reasons: HashMap<String, u64> = HashMap::new();
    let mut unavailable_reasons: HashMap<String, u64> = HashMap::new();
    let mut exit_reasons: HashMap<String, u64> = HashMap::new();

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);

        match kind {
            "candidate" => candidates += 1,
            "missed" => misses += 1,
            "fill_buy" => buys += 1,
            "screen" => {
                let mut fatal: Vec<String> = Vec::new();
                let mut unavailable: Vec<String> = Vec::new();
                if let Some(arr) = data.get("checks").and_then(|c| c.as_array()) {
                    for c in arr {
                        if c.get("passed") != Some(&serde_json::Value::Bool(false)) {
                            continue;
                        }
                        let name = c
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        match c.get("severity").and_then(|s| s.as_str()) {
                            Some("fatal") => fatal.push(name),
                            Some("unavailable") => unavailable.push(name),
                            _ => {} // advisory does not block
                        }
                    }
                }
                if fatal.is_empty() && unavailable.is_empty() {
                    approved += 1;
                } else {
                    for name in fatal {
                        *reject_reasons.entry(name).or_insert(0) += 1;
                    }
                    for name in unavailable {
                        *unavailable_reasons.entry(name).or_insert(0) += 1;
                    }
                }
            }
            "position_close" => {
                closes += 1;
                let p = data.get("pnl_sol").and_then(|x| x.as_f64()).unwrap_or(0.0);
                pnl += p;
                if p > 0.0 {
                    wins += 1;
                }
                if let Some(r) = data.get("reason").and_then(|x| x.as_str()) {
                    *exit_reasons.entry(r.to_string()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }

    println!("\n=== solsnipe journal: {} ===\n", journal_path.display());
    println!("  candidates seen     {candidates}");
    println!("  passed screening    {approved}");
    println!("  entries filled      {buys}");
    println!("  races lost (nofill) {misses}");
    println!("  positions closed    {closes}");
    if closes > 0 {
        println!("  win rate            {:.1}%", (wins as f64 / closes as f64) * 100.0);
        println!("  net PnL             {pnl:+.4} SOL");
        println!("  avg per trade       {:+.4} SOL", pnl / closes as f64);
    }

    if !reject_reasons.is_empty() {
        println!("\n  rejections by check:");
        let mut rows: Vec<_> = reject_reasons.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        for (name, n) in rows {
            println!("    {n:>6}  {name}");
        }
    }
    if !unavailable_reasons.is_empty() {
        let total: u64 = unavailable_reasons.values().sum();
        println!("
  COULD NOT CHECK ({total}) - these are your endpoint, not the tokens:");
        let mut rows: Vec<_> = unavailable_reasons.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        for (name, n) in rows {
            println!("    {n:>6}  {name}");
        }
        if total >= candidates / 2 && candidates > 0 {
            println!("    -> over half of all candidates were unscreenable.");
            println!("       You are not selecting tokens, you are watching an RPC fail.");
            println!("       Fix the endpoint before reading anything else in this report.");
        }
    }

    if !exit_reasons.is_empty() {
        println!("\n  exits by reason:");
        let mut rows: Vec<_> = exit_reasons.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        for (name, n) in rows {
            println!("    {n:>6}  {name}");
        }
    }
    println!();
    Ok(())
}
