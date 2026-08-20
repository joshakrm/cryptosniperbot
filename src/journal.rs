use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::error;

/// Append-only JSONL event log.
///
/// This is not a nice-to-have. A paper run is worthless unless you can go back
/// afterwards and ask why a token was rejected, what the quote said at entry,
/// and how far the price ran after you passed on it. Every decision the bot
/// makes, including the rejections, lands here.
pub struct Journal {
    path: PathBuf,
    file: Mutex<tokio::fs::File>,
}

impl Journal {
    pub async fn open(path: &Path) -> Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("cannot open journal at {}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), file: Mutex::new(file) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn write(&self, kind: &str, payload: Value) {
        let line = json!({
            "ts": Utc::now().to_rfc3339(),
            "kind": kind,
            "data": payload,
        });
        let mut buf = line.to_string();
        buf.push('\n');

        let mut f = self.file.lock().await;
        if let Err(e) = f.write_all(buf.as_bytes()).await {
            error!(error = %e, "journal write failed");
            return;
        }
        // Flushed per record: a crash mid-session must not cost the log that
        // explains the crash.
        if let Err(e) = f.flush().await {
            error!(error = %e, "journal flush failed");
        }
    }

    pub async fn write_typed<T: Serialize>(&self, kind: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(v) => self.write(kind, v).await,
            Err(e) => error!(error = %e, kind, "journal serialise failed"),
        }
    }
}
