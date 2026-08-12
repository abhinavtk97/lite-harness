//! Provider-agnostic model access for the native agent loop (architecture
//! §13): a `ModelProvider` trait plus two built-in protocol
//! implementations (Anthropic Messages API shape, OpenAI-compatible Chat
//! Completions shape) that between them cover the overwhelming majority of
//! "custom base URL" targets -- BYO key, configurable endpoint, not
//! hard-wired to one vendor.

mod anthropic;
mod config;
mod openai_compatible;

pub use anthropic::AnthropicProtocolProvider;
pub use config::{load_providers_file, ModelProviderConfig, ProviderProtocol, ProvidersFile};
pub use openai_compatible::OpenAiCompatibleProvider;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config error: {0}")]
    Config(String),
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;
    fn describe(&self) -> ModelProviderCapabilities;
}

#[derive(Debug, Clone, Copy)]
pub struct ModelProviderCapabilities {
    pub tool_calling: bool,
    pub streaming: bool,
    pub reports_usage: bool,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Vec<ChatContent>,
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: vec![ChatContent::Text(text.into())],
        }
    }
}

#[derive(Debug, Clone)]
pub enum ChatContent {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub content: Vec<ChatContent>,
    pub stop_reason: StopReason,
    pub usage: ModelUsage,
}

impl ModelResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ChatContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|c| match c {
                ChatContent::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Tokens served from a prompt cache instead of being reprocessed --
    /// populated by `AnthropicProtocolProvider` (`cache_read_input_tokens`)
    /// and `OpenAiCompatibleProvider` (`usage.prompt_tokens_details.cached_tokens`).
    pub cache_read_tokens: Option<u64>,
    /// Tokens written into a prompt cache for future reuse. Anthropic's
    /// explicit cache-control blocks report this
    /// (`cache_creation_input_tokens`); OpenAI-compatible APIs cache
    /// automatically/transparently and never report a write count, so this
    /// stays `None` for that protocol always -- not a bug, a real protocol
    /// difference.
    pub cache_write_tokens: Option<u64>,
}

/// Builds the concrete provider for a config entry, reading its API key
/// from the environment variable it names (never from the config file
/// itself -- architecture §13.2).
pub fn build_provider(cfg: &ModelProviderConfig) -> Result<Arc<dyn ModelProvider>> {
    let api_key = std::env::var(&cfg.api_key_env).map_err(|_| {
        ProviderError::Config(format!(
            "environment variable {} is not set (required by provider '{}')",
            cfg.api_key_env, cfg.name
        ))
    })?;

    Ok(match cfg.protocol {
        ProviderProtocol::Anthropic => Arc::new(AnthropicProtocolProvider::new(
            cfg.base_url.clone(),
            api_key,
            cfg.default_model.clone(),
            cfg.extra_headers.clone(),
        )),
        ProviderProtocol::OpenAiCompatible => Arc::new(OpenAiCompatibleProvider::new(
            cfg.base_url.clone(),
            api_key,
            cfg.default_model.clone(),
            cfg.extra_headers.clone(),
        )),
    })
}
