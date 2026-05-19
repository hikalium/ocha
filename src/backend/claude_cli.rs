//! Claude CLI backend — shells out to the locally-installed, already
//! authenticated `claude` (Claude Code) binary in non-interactive print
//! mode.
//!
//! This deliberately sits on the Claude Code *upper* layer, which the
//! design note (`docs/llm-interface-layering-and-common-abstraction.md`)
//! argues against in general. It is acceptable here for one reason and
//! under one strict invariant:
//!
//! - **Why:** the CLI is already logged in (OAuth / subscription), so
//!   this backend needs no `ANTHROPIC_API_KEY` and no billing setup.
//! - **Invariant:** it is used purely as a text-completion engine —
//!   tools are disabled (`--allowed-tools ""`) so Claude Code's own
//!   agent loop can never execute anything, and a `--system-prompt` is
//!   always passed so the large default agent prompt is replaced. ocha's
//!   plain-text `!!!OCHA_RUN_CMD` loop stays the single approval/
//!   execution point exactly as for every other backend.
//!
//! Trade-off to know: each call still carries Claude Code's cached system
//! prompt (~24k tokens), so it is not as cheap as the raw `claude` API
//! backend at the same model.

use super::{Backend, Message, ModelInfo, Role, TokenSink};
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub struct ClaudeCliBackend {
    binary: String,
    model: Option<String>,
}

impl ClaudeCliBackend {
    pub fn new(binary: String, model: Option<String>) -> Self {
        Self { binary, model }
    }
}

/// System turns + the out-of-band system prompt are merged (same split as
/// the `claude` API backend); the remaining turns are flattened to
/// role-labelled text — the lowest-common-denominator "resend accumulated
/// text" form the design note endorses. Returns `(system_prompt, prompt)`.
fn flatten(system: Option<&str>, messages: &[Message]) -> (Option<String>, String) {
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(s) = system {
        system_parts.push(s.to_string());
    }
    let mut turns: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system_parts.push(m.content.clone()),
            Role::User => turns.push(format!("User: {}", m.content)),
            Role::Assistant => turns.push(format!("Assistant: {}", m.content)),
            Role::Tool => turns.push(format!("Tool result: {}", m.content)),
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, turns.join("\n\n"))
}

#[derive(Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct InnerEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<Delta>,
}

/// One NDJSON line of `--output-format stream-json`. Only the fields ocha
/// needs are modelled; everything else (init/status/rate_limit/usage) is
/// ignored.
#[derive(Deserialize)]
struct StreamLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    event: Option<InnerEvent>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    subtype: Option<String>,
}

#[async_trait::async_trait]
impl Backend for ClaudeCliBackend {
    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let (system_prompt, prompt) = flatten(system, messages);

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--verbose") // required by the CLI for stream-json + -p
            .arg("--no-session-persistence")
            .arg("--allowed-tools")
            .arg("") // tools off: Claude Code must not execute anything
            .arg("--system-prompt")
            // Always set one (even empty) so the default agent system
            // prompt is replaced and this stays a plain completion engine.
            .arg(system_prompt.as_deref().unwrap_or(""));
        if let Some(m) = &self.model {
            cmd.arg("--model").arg(m);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn `{}`: {e} (is the Claude Code CLI installed and on PATH?)",
                self.binary
            )
        })?;

        {
            let mut stdin = child.stdin.take().ok_or("failed to open claude stdin")?;
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let stdout = child.stdout.take().ok_or("failed to open claude stdout")?;
        let mut lines = BufReader::new(stdout).lines();

        let mut streamed = String::new();
        let mut result_text: Option<String> = None;
        let mut error: Option<String> = None;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<StreamLine>(line) else {
                continue; // tolerate any line shape ocha doesn't model
            };
            match parsed.kind.as_str() {
                "stream_event" => {
                    if let Some(ev) = parsed.event
                        && ev.kind == "content_block_delta"
                        && let Some(d) = ev.delta
                        && d.kind == "text_delta"
                        && let Some(t) = d.text
                        && !t.is_empty()
                    {
                        on_token(&t);
                        streamed.push_str(&t);
                    }
                }
                "result" => {
                    if parsed.is_error.unwrap_or(false) {
                        error = Some(parsed.result.clone().unwrap_or_else(|| {
                            parsed
                                .subtype
                                .clone()
                                .unwrap_or_else(|| "unknown error".into())
                        }));
                    } else if let Some(r) = parsed.result {
                        result_text = Some(r);
                    }
                }
                _ => {}
            }
        }

        let status = child.wait().await?;
        let mut stderr_buf = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr_buf).await;
        }

        if let Some(err) = error {
            return Err(format!("claude CLI error: {err}").into());
        }
        if !status.success() {
            return Err(format!("claude CLI exited with {status}: {}", stderr_buf.trim()).into());
        }

        // `result` is authoritative (assembled text, thinking excluded);
        // fall back to the streamed text deltas if it was absent.
        Ok(result_text.unwrap_or(streamed))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error>> {
        // The CLI exposes no model-listing endpoint; surface the stable
        // aliases the `--model` flag accepts.
        Ok(["opus", "sonnet", "haiku"]
            .into_iter()
            .map(|a| ModelInfo {
                name: a.to_string(),
                detail: "claude CLI alias (--model)".to_string(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message::new(role, content)
    }

    #[test]
    fn flatten_routes_system_out_of_band_and_labels_turns() {
        let history = [
            msg(Role::System, "in-history sys"),
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
            msg(Role::Tool, "cmd output"),
            msg(Role::User, "thanks"),
        ];
        let (system, prompt) = flatten(Some("oob sys"), &history);
        assert_eq!(system.as_deref(), Some("oob sys\n\nin-history sys"));
        assert_eq!(
            prompt,
            "User: hi\n\nAssistant: hello\n\nTool result: cmd output\n\nUser: thanks"
        );
    }

    #[test]
    fn flatten_no_system_yields_none() {
        let (system, prompt) = flatten(None, &[msg(Role::User, "just this")]);
        assert_eq!(system, None);
        assert_eq!(prompt, "User: just this");
    }
}
