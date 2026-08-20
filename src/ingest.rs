use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::types::Venue;

/// A raw log notification, tagged with the venue whose subscription produced it.
#[derive(Debug, Clone)]
pub struct LogHit {
    pub venue: Venue,
    /// The program whose subscription produced this. Needed downstream to scope
    /// log markers to the program that actually emitted them.
    pub program_id: String,
    pub signature: String,
    pub slot: u64,
    pub logs: Vec<String>,
}

/// Subscribe to logsSubscribe for every watched program and stream the hits.
///
/// Reconnects forever with capped backoff: a sniper that dies on a websocket
/// blip is a sniper that is offline exactly when it matters.
pub async fn run(cfg: Config, tx: mpsc::Sender<LogHit>) -> Result<()> {
    let mut backoff_ms: u64 = 500;

    loop {
        match connect_and_stream(&cfg, &tx).await {
            Ok(()) => {
                warn!("log stream ended, reconnecting");
                backoff_ms = 500;
            }
            Err(e) => {
                warn!(error = %e, backoff_ms, "log stream failed, reconnecting");
            }
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(30_000);
    }
}

async fn connect_and_stream(cfg: &Config, out: &mpsc::Sender<LogHit>) -> Result<()> {
    let watched = cfg.programs.watched()?;

    let (ws, _) = tokio_tungstenite::connect_async(cfg.rpc.ws_url.as_str())
        .await
        .context("websocket connect failed")?;
    let (mut writer, mut reader) = ws.split();

    // Solana allows exactly one address per mentions filter, so each program
    // needs its own subscription on the shared socket.
    let mut pending: HashMap<u64, (Venue, String)> = HashMap::new();
    for (i, (label, program_id)) in watched.iter().enumerate() {
        let req_id = (i + 1) as u64;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "logsSubscribe",
            "params": [
                { "mentions": [program_id] },
                { "commitment": cfg.rpc.commitment }
            ]
        });
        writer
            .send(Message::Text(msg.to_string()))
            .await
            .context("sending logsSubscribe")?;
        pending.insert(req_id, (Venue::from_label(label), program_id.clone()));
        info!(venue = %label, program = %program_id, "subscribing");
    }

    // subscription id -> (venue, program id)
    let mut subs: HashMap<u64, (Venue, String)> = HashMap::new();

    // A half-open TCP connection never returns from next(): no data, no error,
    // no FIN. Unbounded, the reconnect-forever promise in run() silently never
    // fires and the bot goes deaf until a human notices the absence of logs.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
    let mut silent_rounds: u8 = 0;

    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, reader.next()).await {
            Ok(Some(f)) => {
                silent_rounds = 0;
                f.context("websocket read failed")?
            }
            Ok(None) => break,
            Err(_) => {
                silent_rounds += 1;
                if silent_rounds >= 2 {
                    bail!(
                        "no websocket traffic for {}s - treating socket as dead",
                        IDLE_TIMEOUT.as_secs() * 2
                    );
                }
                // Poke it. If the peer is gone the write fails, or the next
                // read times out again and we bail above.
                warn!(secs = IDLE_TIMEOUT.as_secs(), "websocket idle - pinging");
                writer.send(Message::Ping(Vec::new())).await.ok();
                continue;
            }
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Ping(p) => {
                writer.send(Message::Pong(p)).await.ok();
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };

        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "unparseable ws frame");
                continue;
            }
        };

        // Subscription confirmation looks like {"id":1,"result":<sub_id>}
        if let (Some(id), Some(result)) = (
            v.get("id").and_then(|x| x.as_u64()),
            v.get("result").and_then(|x| x.as_u64()),
        ) {
            if let Some((venue, program_id)) = pending.remove(&id) {
                info!(?venue, subscription = result, "subscribed");
                subs.insert(result, (venue, program_id));
            }
            continue;
        }

        // A rejected subscribe carries an `error` and no `result`, so without
        // this branch it falls through both checks and vanishes: that venue is
        // blind for the entire session while the process looks perfectly
        // healthy. One mistyped program id should be loud, not invisible.
        if let (Some(id), Some(err)) = (v.get("id").and_then(|x| x.as_u64()), v.get("error")) {
            let venue = pending.remove(&id).map(|(v, _)| v);
            error!(?venue, error = %err, "logsSubscribe rejected - this venue is blind");
            // Only tear the socket down if nothing subscribed at all. Killing
            // every working venue over one bad program id would turn a partial
            // outage into a total one.
            if pending.is_empty() && subs.is_empty() {
                bail!("every logsSubscribe was rejected: {err}");
            }
            continue;
        }

        if v.get("method").and_then(|m| m.as_str()) != Some("logsNotification") {
            continue;
        }

        let params = match v.get("params") {
            Some(p) => p,
            None => continue,
        };
        let (venue, program_id) = params
            .get("subscription")
            .and_then(|x| x.as_u64())
            .and_then(|id| subs.get(&id).cloned())
            .unwrap_or((Venue::Unknown, String::new()));

        let result = match params.get("result") {
            Some(r) => r,
            None => continue,
        };
        let slot = result
            .get("context")
            .and_then(|c| c.get("slot"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0);

        let value = match result.get("value") {
            Some(x) => x,
            None => continue,
        };

        // Failed transactions never created anything. Drop them here, before
        // they cost us an RPC round trip.
        if !value.get("err").map(|e| e.is_null()).unwrap_or(false) {
            continue;
        }

        let signature = match value.get("signature").and_then(|s| s.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let logs: Vec<String> = value
            .get("logs")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let hit = LogHit { venue, program_id, signature, slot, logs };
        if out.send(hit).await.is_err() {
            return Ok(()); // consumer gone
        }
    }

    Ok(())
}
