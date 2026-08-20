use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::RpcConfig;
use crate::types::{Pubkey, LAMPORTS_PER_SOL};

/// Thin JSON-RPC client. Deliberately untyped at the edges: Solana RPC shapes
/// shift between versions, so we navigate the JSON defensively instead of
/// binding to structs that would silently break.
#[derive(Clone)]
pub struct SolanaRpc {
    client: reqwest::Client,
    url: String,
    commitment: String,
}

impl SolanaRpc {
    pub fn new(cfg: &RpcConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .context("building http client")?;
        Ok(Self {
            client,
            url: cfg.http_url.clone(),
            commitment: cfg.commitment.clone(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let resp: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("rpc {method} request failed"))?
            .json()
            .await
            .with_context(|| format!("rpc {method} returned non-json"))?;

        if let Some(err) = resp.get("error") {
            return Err(anyhow!("rpc {method} error: {err}"));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc {method} returned no result field"))
    }

    /// Full transaction, jsonParsed. Used to pull the mint out of a launch tx.
    pub async fn get_transaction(&self, signature: &str) -> Result<Value> {
        self.call(
            "getTransaction",
            json!([
                signature,
                {
                    "encoding": "jsonParsed",
                    "commitment": self.commitment,
                    "maxSupportedTransactionVersion": 0
                }
            ]),
        )
        .await
    }

    /// Parsed mint account. Carries authorities, supply, decimals, and for
    /// Token-2022, the extension list.
    pub async fn get_mint_account(&self, mint: &Pubkey) -> Result<Value> {
        self.call(
            "getAccountInfo",
            json!([mint, { "encoding": "jsonParsed", "commitment": self.commitment }]),
        )
        .await
    }

    /// Largest holders, with a retry for the index lag at t=0.
    ///
    /// A freshly launched mint is visible to getAccountInfo before it is
    /// visible to this method: the node answers authority queries happily while
    /// still reporting "Invalid param: not a Token mint" here for roughly a
    /// second. Observed live on 33 of 34 pump.fun launches, every one of which
    /// resolved normally moments later.
    ///
    /// Retrying only this specific error beats delaying every candidate,
    /// because most of the time the first attempt already works, and a sniper
    /// pays for every millisecond it waits.
    pub async fn get_token_largest_accounts(&self, mint: &Pubkey) -> Result<Value> {
        const ATTEMPTS: u32 = 4;
        let mut last: Option<anyhow::Error> = None;

        for attempt in 0..ATTEMPTS {
            let result = self
                .call(
                    "getTokenLargestAccounts",
                    json!([mint, { "commitment": self.commitment }]),
                )
                .await;

            match result {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // Anything else is a real failure and must not be masked by
                    // retries - fail fast so it shows up as unavailable.
                    if !e.to_string().contains("not a Token mint") {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }

            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
            }
        }

        Err(last.unwrap_or_else(|| anyhow!("getTokenLargestAccounts never became available")))
    }

    pub async fn get_token_supply(&self, mint: &Pubkey) -> Result<Value> {
        self.call(
            "getTokenSupply",
            json!([mint, { "commitment": self.commitment }]),
        )
        .await
    }
}

/// What a Jupiter request is for.
///
/// The two classes compete for one budget and they are not equally important.
/// Screening coverage decides how many opportunities you ever see; mark
/// freshness only decides how precisely you exit one you already hold.
/// So marks yield.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Screening a candidate. Short interval - these are rare and decisive.
    Screening,
    /// Marking an open position. Long interval - frequent and merely useful.
    Mark,
}

/// Adaptive send budget, shared by every clone of the client.
#[derive(Debug)]
struct Throttle {
    last: Option<Instant>,
    /// Current spacing between requests. Discovered, not configured.
    interval: Duration,
}

/// Jupiter quote client. Two jobs:
///
///   1. price discovery for entry and exit marks
///   2. the honeypot test - if Jupiter cannot route a SELL, you cannot get out
#[derive(Clone)]
pub struct Jupiter {
    client: reqwest::Client,
    base: String,
    /// Shared across every clone, so screening quotes and position marks draw
    /// from ONE budget. A 429 tells us nothing about a token, so the cheapest
    /// fix is not to provoke one: queue instead of firing and failing.
    throttle: Arc<tokio::sync::Mutex<Throttle>>,
    floor: Duration,
    ceiling: Duration,
    mark_multiplier: u32,
}

#[derive(Debug, Clone)]
pub struct Quote {
    /// Raw input amount in the input mint's smallest unit.
    pub in_amount: u64,
    /// Raw output amount in the output mint's smallest unit.
    pub out_amount: u64,
    pub price_impact_pct: f64,
    pub route_hops: usize,
}

impl Jupiter {
    pub fn new(cfg: &RpcConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .context("building jupiter http client")?;
        let floor = Duration::from_millis(cfg.jupiter_min_interval_ms);
        Ok(Self {
            client,
            base: cfg.jupiter_url.trim_end_matches('/').to_string(),
            // Start at the floor and let the endpoint push us back.
            throttle: Arc::new(tokio::sync::Mutex::new(Throttle {
                last: None,
                interval: floor,
            })),
            floor,
            ceiling: Duration::from_millis(cfg.jupiter_max_interval_ms),
            mark_multiplier: cfg.jupiter_mark_multiplier.max(1),
        })
    }

    /// Wait for a turn, at the interval the endpoint has shown it will tolerate.
    ///
    /// The lock is held across the sleep: that is what serialises callers.
    ///
    /// The interval is ADAPTIVE rather than configured, because the real limit
    /// is not documented and changes with the endpoint and the plan. Hand-tuning
    /// it meant guessing: a flat 400ms still lost 21% of screens to 429s, while
    /// a safe guess would throttle a paid endpoint far below what it allows.
    /// Additive-decrease / multiplicative-increase finds the sustainable rate on
    /// its own and re-finds it when conditions change.
    async fn wait_turn(&self, priority: Priority) {
        let mut t = self.throttle.lock().await;

        // Marks measure against the same clock as screening but need a wider
        // gap, so a mark naturally stands aside for a screening quote.
        let mut need = t.interval;
        if priority == Priority::Mark {
            need *= self.mark_multiplier;
        }
        if need.is_zero() {
            return;
        }

        if let Some(prev) = t.last {
            let elapsed = prev.elapsed();
            if elapsed < need {
                tokio::time::sleep(need - elapsed).await;
            }
        }
        t.last = Some(Instant::now());
    }

    /// Back off hard. Being throttled costs a whole candidate, so the response
    /// should be immediate rather than gradual.
    async fn note_throttled(&self) {
        let mut t = self.throttle.lock().await;
        let doubled = t.interval.checked_mul(2).unwrap_or(self.ceiling);
        t.interval = doubled.min(self.ceiling).max(self.floor);
    }

    /// Creep back towards the floor while requests are landing cleanly.
    /// Deliberately slower than the back-off: probing too eagerly just
    /// reproduces the 429 we backed off from.
    async fn note_accepted(&self) {
        let mut t = self.throttle.lock().await;
        t.interval = t
            .interval
            .saturating_sub(Duration::from_millis(20))
            .max(self.floor);
    }

    /// Current spacing, for logging and tests.
    pub async fn current_interval(&self) -> Duration {
        self.throttle.lock().await.interval
    }

    /// Returns Ok(None) when Jupiter has no route at all - which for a sell is
    /// the single loudest honeypot signal you can get.
    pub async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u64,
        priority: Priority,
    ) -> Result<Option<Quote>> {
        let url = format!("{}/quote", self.base);
        // One retry: a throttle should cost a delay, not a whole candidate.
        const ATTEMPTS: u32 = 2;

        for attempt in 0..ATTEMPTS {
            self.wait_turn(priority).await;

            let resp = self
                .client
                .get(&url)
                .query(&[
                    ("inputMint", input_mint),
                    ("outputMint", output_mint),
                    ("amount", &amount.to_string()),
                    ("slippageBps", &slippage_bps.to_string()),
                    ("onlyDirectRoutes", "false"),
                ])
                .send()
                .await
                .context("jupiter quote request failed")?;

            let status = resp.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                self.note_throttled().await;
                if attempt + 1 < ATTEMPTS {
                    continue;
                }
                return Err(anyhow!("jupiter quote http 429 after {ATTEMPTS} attempts"));
            }

            self.note_accepted().await;

            // Classify the status; never collapse it. A 5xx says nothing
            // whatsoever about whether a route exists, and reporting that as
            // "no route" is precisely how aggregator trouble turns into
            // positions force-closed at a fabricated -100%. Only Jupiter's own
            // "I looked and there is nothing" answers may become Ok(None).
            if status == reqwest::StatusCode::BAD_REQUEST
                || status == reqwest::StatusCode::NOT_FOUND
            {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(anyhow!("jupiter quote http {status}"));
            }

            let v: Value = resp.json().await.context("jupiter quote non-json")?;
            if v.get("error").is_some() {
                return Ok(None);
            }

            let in_amount = v
                .get("inAmount")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok());
            let out_amount = v
                .get("outAmount")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok());

            let (in_amount, out_amount) = match (in_amount, out_amount) {
                (Some(i), Some(o)) if o > 0 => (i, o),
                _ => return Ok(None),
            };

            let price_impact_pct = v
                .get("priceImpactPct")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| v.get("priceImpactPct").and_then(|x| x.as_f64()))
                .unwrap_or(0.0);

            let route_hops = v
                .get("routePlan")
                .and_then(|x| x.as_array())
                .map(|a| a.len())
                .unwrap_or(0);

            return Ok(Some(Quote {
                in_amount,
                out_amount,
                price_impact_pct,
                route_hops,
            }));
        }

        Err(anyhow!("jupiter quote exhausted its attempts"))
    }
}

/// Where a position mark comes from.
///
/// A trait rather than a concrete Jupiter call so the position manager can be
/// exercised without a network. The three outcomes are deliberately distinct
/// and must stay that way:
///
///   Ok(Some(price)) - a real, sellable price
///   Ok(None)        - no sell route exists; evidence ABOUT the token
///   Err(_)          - the lookup failed; evidence about NOTHING
///
/// Collapsing the last two lets an aggregator outage flatten the whole book.
#[async_trait::async_trait]
pub trait PriceSource: Send + Sync {
    async fn mark(&self, mint: &str, tokens_held: f64, decimals: u8) -> Result<Option<f64>>;
}

/// Live marks, priced against the size actually held so the number includes the
/// impact of the exit we would really have to perform.
pub struct JupiterPrices {
    jup: Jupiter,
    wsol: String,
}

impl JupiterPrices {
    pub fn new(jup: Jupiter, wsol: String) -> Self {
        Self { jup, wsol }
    }
}

#[async_trait::async_trait]
impl PriceSource for JupiterPrices {
    async fn mark(&self, mint: &str, tokens_held: f64, decimals: u8) -> Result<Option<f64>> {
        let raw = ui_to_raw(tokens_held, decimals);
        if raw == 0 || tokens_held <= 0.0 {
            return Ok(None);
        }
        match self
            .jup
            .quote(mint, &self.wsol, raw, 500, Priority::Mark)
            .await?
        {
            Some(q) => {
                let sol_out = q.out_amount as f64 / LAMPORTS_PER_SOL;
                Ok(Some(sol_out / tokens_held))
            }
            None => Ok(None),
        }
    }
}

/// Convert a raw token amount to whole tokens.
pub fn raw_to_ui(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

/// Convert whole tokens to a raw amount.
pub fn ui_to_raw(ui: f64, decimals: u8) -> u64 {
    (ui * 10f64.powi(decimals as i32)).round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_ui_amounts_round_trip() {
        assert_eq!(raw_to_ui(1_000_000, 6), 1.0);
        assert_eq!(ui_to_raw(1.0, 6), 1_000_000);
        assert_eq!(ui_to_raw(raw_to_ui(123_456_789, 9), 9), 123_456_789);
    }

    #[test]
    fn zero_decimals_is_the_identity() {
        assert_eq!(raw_to_ui(42, 0), 42.0);
        assert_eq!(ui_to_raw(42.0, 0), 42);
    }

    #[test]
    fn negative_ui_amounts_clamp_to_zero_rather_than_wrapping() {
        // `as u64` on a negative float would otherwise saturate oddly; the
        // caller passing a negative size is a bug, but it must not become a
        // gigantic order.
        assert_eq!(ui_to_raw(-5.0, 6), 0);
    }
}

#[cfg(test)]
mod jupiter_http_tests {
    use super::*;

    /// Minimal one-shot HTTP server. The distinction under test is purely about
    /// status codes, so a real socket is the only honest way to exercise it.
    async fn stub(status_line: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}")
    }

    fn jup(base: String) -> Jupiter {
        jup_throttled(base, 0)
    }

    fn jup_throttled(base: String, min_interval_ms: u64) -> Jupiter {
        let floor = Duration::from_millis(min_interval_ms);
        Jupiter {
            client: reqwest::Client::new(),
            base,
            throttle: Arc::new(tokio::sync::Mutex::new(Throttle {
                last: None,
                interval: floor,
            })),
            floor,
            ceiling: Duration::from_millis(4_000),
            mark_multiplier: 1,
        }
    }

    /// Serve `n` sequential requests from one listener.
    async fn stub_n(status_line: &'static str, body: &'static str, n: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..n {
                if let Ok((mut sock, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status_line}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                }
            }
        });
        format!("http://{addr}")
    }

    // Position marks vastly outnumber screening quotes, so without a shared
    // budget the marks starve the screening and candidates come back
    // unscreenable. Observed live: 33% of screens degraded by 429s.
    #[tokio::test]
    async fn requests_are_spaced_by_the_configured_interval() {
        let body = r#"{"inAmount":"1","outAmount":"2"}"#;
        let base = stub_n("200 OK", body, 3).await;
        let j = jup_throttled(base, 120);

        let started = Instant::now();
        for _ in 0..3 {
            let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;
        }
        let elapsed = started.elapsed();

        // Three requests at 120ms spacing cannot finish faster than 240ms.
        assert!(
            elapsed >= Duration::from_millis(220),
            "three throttled requests took only {elapsed:?}"
        );
    }

    /// Serve a scripted sequence of responses, one per connection.
    async fn stub_seq(responses: Vec<(&'static str, &'static str)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status_line, body) in responses {
                if let Ok((mut sock, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status_line}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                }
            }
        });
        format!("http://{addr}")
    }

    const OK_BODY: &str = r#"{"inAmount":"1","outAmount":"2"}"#;

    // A throttled request costs a whole candidate, so the response has to be
    // immediate rather than gradual.
    #[tokio::test]
    async fn being_throttled_widens_the_interval() {
        let base = stub_seq(vec![("429 Too Many Requests", "{}"); 2]).await;
        let j = jup_throttled(base, 10);
        let before = j.current_interval().await;

        let r = j.quote("A", "B", 1, 500, Priority::Screening).await;
        assert!(r.is_err(), "two 429s in a row must surface as an error");

        let after = j.current_interval().await;
        assert!(after > before, "interval went {before:?} -> {after:?}");
    }

    // A 429 should cost a delay, not the candidate. The measured failure mode
    // was 21-33% of screens lost outright to throttling.
    #[tokio::test]
    async fn a_transient_throttle_is_retried_and_succeeds() {
        let base = stub_seq(vec![("429 Too Many Requests", "{}"), ("200 OK", OK_BODY)]).await;
        let j = jup_throttled(base, 10);

        let q = j.quote("A", "B", 1, 500, Priority::Screening).await;
        assert!(
            matches!(q, Ok(Some(_))),
            "the retry should have recovered the candidate, got {q:?}"
        );
    }

    #[tokio::test]
    async fn clean_requests_creep_back_towards_the_floor() {
        let base = stub_seq(vec![("200 OK", OK_BODY); 6]).await;
        let j = jup_throttled(base, 10);

        // Push the interval up first.
        j.note_throttled().await;
        j.note_throttled().await;
        let backed_off = j.current_interval().await;

        for _ in 0..5 {
            let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;
        }
        let recovered = j.current_interval().await;

        assert!(
            recovered < backed_off,
            "interval should relax while requests land: {backed_off:?} -> {recovered:?}"
        );
    }

    #[tokio::test]
    async fn back_off_is_bounded_by_the_ceiling() {
        let base = stub_seq(vec![]).await;
        let j = jup_throttled(base, 10);
        for _ in 0..40 {
            j.note_throttled().await;
        }
        assert!(j.current_interval().await <= j.ceiling);
    }

    // The whole point of the two classes: raising concurrency must cost mark
    // freshness, never screening coverage. Measured live, marks competing
    // equally with screening cost 22-33% of screens to 429s.
    #[tokio::test]
    async fn marks_yield_to_screening() {
        let body = r#"{"inAmount":"1","outAmount":"2"}"#;
        let base = stub_n("200 OK", body, 4).await;
        let mut j = jup_throttled(base, 40);
        j.mark_multiplier = 10;

        // Prime the gate, then time each class from the same starting point.
        let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;

        let t = Instant::now();
        let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;
        let screening = t.elapsed();

        let t = Instant::now();
        let _ = j.quote("A", "B", 1, 500, Priority::Mark).await;
        let mark = t.elapsed();

        assert!(
            mark > screening * 3,
            "a mark ({mark:?}) must wait far longer than a screening quote ({screening:?})"
        );
    }

    #[tokio::test]
    async fn a_zero_interval_disables_the_throttle() {
        let body = r#"{"inAmount":"1","outAmount":"2"}"#;
        let base = stub_n("200 OK", body, 2).await;
        let j = jup_throttled(base, 0);
        let started = Instant::now();
        let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;
        let _ = j.quote("A", "B", 1, 500, Priority::Screening).await;
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    // REGRESSION: every non-2xx status collapsed into Ok(None) "no route". A
    // throttled aggregator then reached the position manager as Mark::NoRoute,
    // and three such sweeps force-closed a perfectly healthy position at a
    // fabricated -100%. Jupiter throttling is measured, not hypothetical.
    #[tokio::test]
    async fn throttling_is_an_error_not_an_absent_route() {
        let base = stub("429 Too Many Requests", r#"{"error":"rate limited"}"#).await;
        let r = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await;
        assert!(r.is_err(), "429 must not be reported as no-route, got {r:?}");
    }

    #[tokio::test]
    async fn a_server_error_is_an_error_not_an_absent_route() {
        let base = stub("503 Service Unavailable", "{}").await;
        let r = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await;
        assert!(r.is_err(), "5xx must not be reported as no-route, got {r:?}");
    }

    #[tokio::test]
    async fn a_bad_request_really_does_mean_no_route() {
        let base = stub("400 Bad Request", r#"{"error":"COULD_NOT_FIND_ANY_ROUTE"}"#).await;
        let r = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await;
        assert!(matches!(r, Ok(None)), "400 is Jupiter's real no-route answer, got {r:?}");
    }

    #[tokio::test]
    async fn an_error_body_on_a_200_is_still_no_route() {
        let base = stub("200 OK", r#"{"error":"COULD_NOT_FIND_ANY_ROUTE"}"#).await;
        let r = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await;
        assert!(matches!(r, Ok(None)), "got {r:?}");
    }

    #[tokio::test]
    async fn a_well_formed_quote_parses() {
        let body = r#"{"inAmount":"1000","outAmount":"2500","priceImpactPct":"0.01","routePlan":[{},{}]}"#;
        let base = stub("200 OK", body).await;
        let q = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await.unwrap().unwrap();
        assert_eq!(q.in_amount, 1_000);
        assert_eq!(q.out_amount, 2_500);
        assert_eq!(q.route_hops, 2);
        assert!((q.price_impact_pct - 0.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_zero_output_quote_is_no_route() {
        let base = stub("200 OK", r#"{"inAmount":"1000","outAmount":"0"}"#).await;
        let r = jup(base).quote("A", "B", 1_000, 500, Priority::Screening).await;
        assert!(matches!(r, Ok(None)), "got {r:?}");
    }
}
