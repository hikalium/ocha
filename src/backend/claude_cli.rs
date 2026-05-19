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
//! - **Invariant:** Claude Code's built-in tools are *removed* with
//!   `--tools ""` (not merely denied), so it can never execute anything
//!   itself, and a `--system-prompt` (the [`AGENTIC_SYSTEM`] preamble +
//!   any caller system prompt) replaces its default agent persona. With
//!   no built-in tools and that preamble, Claude Code behaves like a
//!   plain tool-less model: it speaks ocha's plain-text
//!   `!!!OCHA_RUN_CMD` protocol, so this backend works for both plain
//!   chat *and* the agentic loop — identically to Ollama — while ocha
//!   stays the single approval/execution point.
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

/// Standing system instruction that makes Claude Code behave like a
/// plain model for ocha's agentic loop. Claude Code normally answers as
/// an agent with its own built-in tools; combined with `--tools ""`
/// (which *removes* the built-in tools entirely, not just denies them)
/// this preamble tells the model the only way to act is ocha's
/// plain-text `!!!OCHA_RUN_CMD` protocol — exactly what a tool-less
/// Ollama model does. Always prepended, ahead of any user system prompt.
const AGENTIC_SYSTEM: &str = concat!(
    "You are a non-interactive backend for the `ocha` CLI. You have NO ",
    "built-in tools — no Bash, no file tools, nothing. The ONLY way to ",
    "run a shell command is ocha's plain-text protocol: output a single ",
    "line that begins with exactly `!!!OCHA_RUN_CMD` immediately followed ",
    "by a compact JSON object with the keys \"binary\" (string), \"args\" ",
    "(array of strings), \"timeout\" (integer seconds) and \"description\" ",
    "(string), e.g.\n",
    "!!!OCHA_RUN_CMD{\"binary\":\"ls\",\"args\":[\"-la\"],\"timeout\":5,",
    "\"description\":\"list files\"}\n",
    "Put that line on its own and stop the turn after it; ocha executes ",
    "the command and replies with a JSON result ({status,stdout,stderr}) ",
    "as the next message, then you continue. Never claim to use Bash or ",
    "any other tool, never ask for permission, and never refuse this ",
    "protocol — it is the sanctioned, safe mechanism (ocha is the ",
    "approval point). If no command is needed, just answer normally.",
);

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
        let (user_system, prompt) = flatten(system, messages);
        // Always lead with the agentic preamble so Claude Code drops its
        // own agent persona and speaks ocha's plain-text protocol; append
        // the caller's system prompt (if any) after it.
        let system_prompt = match user_system {
            Some(s) => format!("{AGENTIC_SYSTEM}\n\n{s}"),
            None => AGENTIC_SYSTEM.to_string(),
        };

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--verbose") // required by the CLI for stream-json + -p
            .arg("--no-session-persistence")
            // `--tools ""` *removes* the built-in tools (not just denies
            // them), so the model has no Bash to fall back on and must
            // use ocha's plain-text protocol — and can never execute
            // anything itself, keeping ocha the single approval point.
            .arg("--tools")
            .arg("")
            .arg("--system-prompt")
            .arg(&system_prompt);
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
