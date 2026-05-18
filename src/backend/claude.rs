//! Claude backend — Anthropic Messages API (`/v1/messages`, SSE streaming)
//! plus `/v1/models`. ocha owns its own agent loop, so this speaks the raw
//! Messages API directly (the most stable, best-documented layer); the
//! agentic command protocol stays as plain text, exactly like every other
//! backend.

use super::{Backend, Message, ModelInfo, Role, TokenSink};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

const API_VERSION: &str = "2023-06-01";

pub struct ClaudeBackend {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl ClaudeBackend {
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
        max_tokens: u32,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key,
            model,
            max_tokens,
        }
    }
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ApiModel>,
}

#[async_trait::async_trait]
impl Backend for ClaudeBackend {
    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        // Anthropic carries the system prompt out of band. Any System-role
        // turns in the history are merged into it; the rest map to the two
        // chat roles (a text tool-result is just another user turn).
        let mut system_parts: Vec<String> = Vec::new();
        if let Some(s) = system {
            system_parts.push(s.to_string());
        }
        let mut wire: Vec<WireMessage> = Vec::with_capacity(messages.len());
        for m in messages {
            match m.role {
                Role::System => system_parts.push(m.content.clone()),
                Role::Assistant => wire.push(WireMessage {
                    role: "assistant",
                    content: m.content.clone(),
                }),
                Role::User | Role::Tool => wire.push(WireMessage {
                    role: "user",
                    content: m.content.clone(),
                }),
            }
        }

        let body = MessagesRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            stream: true,
            system: if system_parts.is_empty() {
                None
            } else {
                Some(system_parts.join("\n\n"))
            },
            messages: wire,
        };

        let url = format!("{}/v1/messages", self.base_url);
        let res = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?;
        if !res.status().is_success() {
            let err = res.text().await?;
            return Err(format!("Anthropic API error: {}", err).into());
        }

        let mut stream = res.bytes_stream();
        let mut full = String::new();
        let mut line_buf: Vec<u8> = Vec::new();

        while let Some(chunk) = stream.next().await {
            line_buf.extend_from_slice(&chunk?);
            while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = line_buf.drain(..=nl).collect();
                let line = std::str::from_utf8(&line)?.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue; // skip `event:` lines and blanks
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                let event: StreamEvent = serde_json::from_str(data)?;
                match event.kind.as_str() {
                    "content_block_delta" => {
                        if let Some(text) = event.delta.and_then(|d| d.text)
                            && !text.is_empty()
                        {
                            on_token(&text);
                            full.push_str(&text);
                        }
                    }
                    "error" => {
                        let detail = event
                            .error
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        return Err(format!("Anthropic stream error: {}", detail).into());
                    }
                    _ => {}
                }
            }
        }
        Ok(full)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error>> {
        let url = format!("{}/v1/models", self.base_url);
        let res = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .send()
            .await?;
        if !res.status().is_success() {
            let err = res.text().await?;
            return Err(format!("Anthropic API error: {}", err).into());
        }
        let resp: ModelsResponse = res.json().await?;
        Ok(resp
            .data
            .into_iter()
            .map(|m| ModelInfo {
                detail: m.display_name.unwrap_or_default(),
                name: m.id,
            })
            .collect())
    }
}
