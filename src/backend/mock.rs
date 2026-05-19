//! Test-only, in-process mock backend.
//!
//! Selected at runtime when `OCHA_MOCK_BACKEND=1`, so the spawned
//! `ocha serve` integration tests are **hermetic** — no network, no real
//! model, deterministic. Scriptable via env:
//!
//! - `OCHA_MOCK_REPLY` — the assistant text `chat` streams + returns
//!   (default `"mock reply"`). Multiple turns within one `run_turn`
//!   loop (e.g. a `!!!OCHA_RUN_CMD` line, then a follow-up after the
//!   tool result) are separated by the literal `<<<NEXT>>>`; the *i*-th
//!   `chat` call returns the *i*-th part, the last part repeating.
//!   Streamed in whitespace chunks to exercise the token path.
//!
//! Not wired into the CLI surface (no `--backend mock`); only reachable
//! through the env switch, so it never appears in normal usage.

use super::{Backend, Message, ModelInfo, TokenSink};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub struct MockBackend {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Backend for MockBackend {
    async fn chat(
        &self,
        _system: Option<&str>,
        _messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let script = std::env::var("OCHA_MOCK_REPLY").unwrap_or_else(|_| "mock reply".to_string());
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let parts: Vec<&str> = script.split("<<<NEXT>>>").collect();
        let part = parts[i.min(parts.len() - 1)];
        let mut full = String::new();
        for (j, tok) in part.split(' ').enumerate() {
            let frag = if j == 0 {
                tok.to_string()
            } else {
                format!(" {tok}")
            };
            on_token(&frag);
            full.push_str(&frag);
        }
        Ok(full)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error>> {
        Ok(vec![ModelInfo {
            name: "mock-model".to_string(),
            detail: "in-process test mock".to_string(),
        }])
    }
}
