//! Ollama backend — native `/api/chat` (NDJSON streaming) + `/api/tags`.

use super::{Backend, Message, ModelInfo, Role, TokenSink};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

pub struct OllamaBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaBackend {
    pub fn new(client: reqwest::Client, base_url: String, model: String) -> Self {
        Self {
            client,
            base_url,
            model,
        }
    }
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
}

#[derive(Deserialize)]
struct ChatChunkMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChatChunkMessage>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

/// The agentic protocol is plain text, so a `Tool` result is just another
/// user turn. Everything maps onto Ollama's standard chat roles.
fn wire_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
    }
}

#[async_trait::async_trait]
impl Backend for OllamaBackend {
    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut wire: Vec<WireMessage> = Vec::with_capacity(messages.len() + 1);
        if let Some(sys) = system {
            wire.push(WireMessage {
                role: "system",
                content: sys,
            });
        }
        for m in messages {
            wire.push(WireMessage {
                role: wire_role(m.role),
                content: &m.content,
            });
        }

        let body = ChatRequest {
            model: &self.model,
            messages: wire,
            stream: true,
        };

        let url = format!("{}/api/chat", self.base_url);
        let res = self.client.post(&url).json(&body).send().await?;
        if !res.status().is_success() {
            let err = res.text().await?;
            return Err(format!("Ollama API error: {}", err).into());
        }

        let mut stream = res.bytes_stream();
        let mut full = String::new();
        let mut line_buf: Vec<u8> = Vec::new();

        while let Some(chunk) = stream.next().await {
            line_buf.extend_from_slice(&chunk?);
            while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = line_buf.drain(..=nl).collect();
                let line = std::str::from_utf8(&line)?.trim();
                if line.is_empty() {
                    continue;
                }
                let parsed: ChatChunk = serde_json::from_str(line)?;
                if let Some(msg) = parsed.message
                    && !msg.content.is_empty()
                {
                    on_token(&msg.content);
                    full.push_str(&msg.content);
                }
            }
        }
        Ok(full)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error>> {
        let url = format!("{}/api/tags", self.base_url);
        let res = self.client.get(&url).send().await?;
        if !res.status().is_success() {
            let err = res.text().await?;
            return Err(format!("Ollama API error: {}", err).into());
        }
        let resp: TagsResponse = res.json().await?;
        Ok(resp
            .models
            .into_iter()
            .map(|m| ModelInfo {
                name: m.name,
                detail: format!(
                    "{:.2} GB  {}",
                    m.size as f64 / 1_073_741_824.0,
                    m.modified_at
                ),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_role_collapses_tool_to_user_keeps_others() {
        // The agentic protocol is plain text, so a Tool result is just
        // another user turn — every backend must collapse it consistently.
        assert_eq!(wire_role(Role::System), "system");
        assert_eq!(wire_role(Role::User), "user");
        assert_eq!(wire_role(Role::Assistant), "assistant");
        assert_eq!(wire_role(Role::Tool), "user");
    }
}
