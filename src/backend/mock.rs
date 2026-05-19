//! Test-only, in-process mock backend.
//!
//! Selected at runtime when `OCHA_MOCK_BACKEND=1`, so the spawned
//! `ocha serve` integration tests are **hermetic** — no network, no real
//! model, deterministic. Scriptable via env:
//!
//! - `OCHA_MOCK_REPLY` — the assistant text `chat` streams + returns
//!   (default `"mock reply"`). Streamed in whitespace chunks to exercise
//!   the token path; may contain a `!!!OCHA_RUN_CMD{…}` line for the
//!   approval-gate tests (M4).
//!
//! Not wired into the CLI surface (no `--backend mock`); only reachable
//! through the env switch, so it never appears in normal usage.

use super::{Backend, Message, ModelInfo, TokenSink};

pub struct MockBackend;

#[async_trait::async_trait]
impl Backend for MockBackend {
    async fn chat(
        &self,
        _system: Option<&str>,
        _messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let reply = std::env::var("OCHA_MOCK_REPLY").unwrap_or_else(|_| "mock reply".to_string());
        let mut full = String::new();
        for (i, tok) in reply.split(' ').enumerate() {
            let frag = if i == 0 {
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
