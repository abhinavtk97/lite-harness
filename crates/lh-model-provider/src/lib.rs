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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
}

#[derive(Deserialize)]
struct OpenAiModelsListResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct AnthropicModelsListResponse {
    data: Vec<AnthropicModelEntry>,
}

#[derive(Deserialize)]
struct AnthropicModelEntry {
    id: String,
}

/// Auto-discovers the models a provider config's endpoint currently
/// serves, dispatching on protocol the same way `build_provider` does --
/// a free function rather than a `ModelProvider` trait method, since
/// listing is a capability of the provider *config* (an endpoint + key),
/// not something that requires an already-constructed single-model
/// instance.
pub async fn list_models(cfg: &ModelProviderConfig) -> Result<Vec<ModelInfo>> {
    let api_key = std::env::var(&cfg.api_key_env).map_err(|_| {
        ProviderError::Config(format!(
            "environment variable {} is not set (required by provider '{}')",
            cfg.api_key_env, cfg.name
        ))
    })?;

    let client = reqwest::Client::new();
    match cfg.protocol {
        ProviderProtocol::OpenAiCompatible => {
            // `base_url` already ends in `/v1` (the same convention
            // `OpenAiCompatibleProvider::complete` relies on for
            // `/chat/completions`), so the list URL is just `/models`.
            let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
            let resp = client.get(&url).bearer_auth(&api_key).send().await?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api { status, body });
            }
            let parsed: OpenAiModelsListResponse = resp.json().await?;
            Ok(parsed.data.into_iter().map(|e| ModelInfo { id: e.id }).collect())
        }
        ProviderProtocol::Anthropic => {
            // `base_url` does *not* include `/v1` (mirroring how
            // `AnthropicProtocolProvider::complete` builds `/v1/messages`).
            let url = format!("{}/v1/models", cfg.base_url.trim_end_matches('/'));
            let resp = client
                .get(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api { status, body });
            }
            let parsed: AnthropicModelsListResponse = resp.json().await?;
            Ok(parsed.data.into_iter().map(|e| ModelInfo { id: e.id }).collect())
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(protocol: ProviderProtocol, base_url: String) -> ModelProviderConfig {
        ModelProviderConfig {
            name: "test".to_string(),
            protocol,
            base_url,
            api_key_env: "LIST_MODELS_TEST_KEY".to_string(),
            default_model: "whatever".to_string(),
            extra_headers: HashMap::new(),
            context_window: None,
        }
    }

    #[tokio::test]
    async fn lists_models_from_an_open_ai_compatible_endpoint() {
        let server = MockServer::start().await;
        std::env::set_var("LIST_MODELS_TEST_KEY", "sk-test");

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "llama3"}, {"id": "mixtral"}]
            })))
            .mount(&server)
            .await;

        let config = cfg(ProviderProtocol::OpenAiCompatible, format!("{}/v1", server.uri()));
        let models = list_models(&config).await.unwrap();
        assert_eq!(models, vec![ModelInfo { id: "llama3".to_string() }, ModelInfo { id: "mixtral".to_string() }]);
    }

    #[tokio::test]
    async fn lists_models_from_an_anthropic_endpoint() {
        let server = MockServer::start().await;
        std::env::set_var("LIST_MODELS_TEST_KEY", "sk-test");

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-sonnet-5"}, {"id": "claude-opus-5"}]
            })))
            .mount(&server)
            .await;

        let config = cfg(ProviderProtocol::Anthropic, server.uri());
        let models = list_models(&config).await.unwrap();
        assert_eq!(
            models,
            vec![ModelInfo { id: "claude-sonnet-5".to_string() }, ModelInfo { id: "claude-opus-5".to_string() }]
        );
    }

    #[tokio::test]
    async fn a_non_2xx_models_list_response_is_a_clear_api_error() {
        let server = MockServer::start().await;
        std::env::set_var("LIST_MODELS_TEST_KEY", "sk-test");

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let config = cfg(ProviderProtocol::OpenAiCompatible, format!("{}/v1", server.uri()));
        let err = list_models(&config).await.unwrap_err();
        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_api_key_env_var_is_a_clear_config_error() {
        std::env::remove_var("LIST_MODELS_TEST_KEY_UNSET");
        let mut config = cfg(ProviderProtocol::OpenAiCompatible, "http://127.0.0.1:1/v1".to_string());
        config.api_key_env = "LIST_MODELS_TEST_KEY_UNSET".to_string();

        let err = list_models(&config).await.unwrap_err();
        match err {
            ProviderError::Config(_) => {}
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
