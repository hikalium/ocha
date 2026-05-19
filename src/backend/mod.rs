//! Provider-neutral backend abstraction.
//!
//! Every backend speaks the same minimal contract: a list of role-tagged
//! text messages (plus an optional out-of-band system prompt) in, a stream
//! of text tokens out. Multi-turn is achieved by resending the accumulated
//! history each turn — the lowest common denominator that works across
//! Ollama, the Anthropic Messages API and (later) Gemini. The agentic
//! command protocol lives one layer up in the turn loop and is therefore
//! backend-independent.

use serde::{Deserialize, Serialize};

pub mod claude;
pub mod claude_cli;
pub mod mock;
pub mod ollama;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// Persisted conversation. Stored as a neutral message list so a session
/// can be continued against any backend. (Old `{ "context": [...] }` files
/// from the Ollama-only era deserialize to an empty history and simply
/// start fresh.)
#[derive(Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(default)]
    pub messages: Vec<Message>,
}

pub struct ModelInfo {
    pub name: String,
    pub detail: String,
}

/// A token sink: backends call this with each text fragment as it streams.
pub type TokenSink<'a> = dyn FnMut(&str) + Send + 'a;

#[async_trait::async_trait]
pub trait Backend {
    /// Run one turn: send `system` + `messages`, stream text to `on_token`,
    /// and return the full assembled assistant text.
    async fn chat(
        &self,
        system: Option<&str>,
        messages: &[Message],
        on_token: &mut TokenSink<'_>,
    ) -> Result<String, Box<dyn std::error::Error>>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn std::error::Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        // Every backend's wire mapping and the persisted session depend on
        // this exact lowercase spelling.
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn message_round_trips_through_json() {
        let m = Message::new(Role::Tool, "result payload");
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, Role::Tool);
        assert_eq!(back.content, "result payload");
    }

    #[test]
    fn session_round_trips_neutral_message_history() {
        let s = Session {
            messages: vec![
                Message::new(Role::User, "hi"),
                Message::new(Role::Assistant, "hello"),
            ],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[0].role, Role::User);
        assert_eq!(back.messages[1].content, "hello");
    }

    #[test]
    fn legacy_ollama_context_file_deserializes_to_empty_history() {
        // Documented contract: old `{ "context": [...] }` Ollama-era
        // sessions are ignored (unknown field) and simply start fresh.
        let legacy = r#"{ "context": [1, 2, 3, 4] }"#;
        let s: Session = serde_json::from_str(legacy).unwrap();
        assert!(s.messages.is_empty());
    }

    #[test]
    fn empty_object_deserializes_to_default_session() {
        let s: Session = serde_json::from_str("{}").unwrap();
        assert!(s.messages.is_empty());
    }
}
