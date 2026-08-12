//! OpenAI-compatible Chat Completions protocol implementation (architecture
//! §13.1) -- the de facto standard almost every self-hosted/proxy/local
//! model server also speaks (Ollama, vLLM, LM Studio, OpenRouter, Azure
//! OpenAI, LiteLLM proxy, and others), which is why this one protocol
//! implementation covers most "custom base URL" targets.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    ChatContent, ChatRole, ModelProvider, ModelProviderCapabilities, ModelRequest, ModelResponse,
    ModelUsage, ProviderError, Result, StopReason,
};

pub struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    extra_headers: HashMap<String, String>,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        extra_headers: HashMap<String, String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            extra_headers,
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        let wire = to_wire_request(&req, &self.model);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&wire);
        for (k, v) in &self.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        let resp = builder.send().await?;
        let status = resp.status();
        let body_text = resp.text().await?;

        if !status.is_success() {
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let wire_resp: WireResponse = serde_json::from_str(&body_text)?;
        Ok(from_wire_response(wire_resp))
    }

    fn describe(&self) -> ModelProviderCapabilities {
        ModelProviderCapabilities {
            tool_calling: true,
            streaming: false,
            reports_usage: true,
        }
    }
}

#[derive(Serialize)]
struct WireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    max_tokens: u32,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    r#type: &'static str,
    function: WireFunctionCall,
}

#[derive(Serialize)]
struct WireFunctionCall {
    name: String,
    /// OpenAI's API represents tool-call arguments as a JSON-encoded
    /// *string*, not a nested object -- unlike Anthropic's `input`.
    arguments: String,
}

#[derive(Serialize)]
struct WireTool {
    r#type: &'static str,
    function: WireFunctionSpec,
}

#[derive(Serialize)]
struct WireFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Flattens the neutral `ChatMessage`/`ChatContent` shape into OpenAI's
/// per-message convention: one `assistant` message per turn (with
/// `tool_calls` attached), and one separate `role: "tool"` message per
/// tool result rather than Anthropic's single grouped `user` message.
fn to_wire_request(req: &ModelRequest, model: &str) -> WireRequest {
    let mut messages = Vec::new();

    if let Some(system) = &req.system {
        messages.push(WireMessage {
            role: "system",
            content: Some(system.clone()),
            tool_calls: vec![],
            tool_call_id: None,
        });
    }

    for m in &req.messages {
        let role = match m.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for c in &m.content {
            match c {
                ChatContent::Text(t) => text_parts.push(t.clone()),
                ChatContent::ToolUse { id, name, input } => tool_calls.push(WireToolCall {
                    id: id.clone(),
                    r#type: "function",
                    function: WireFunctionCall {
                        name: name.clone(),
                        arguments: input.to_string(),
                    },
                }),
                ChatContent::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    // Each tool result is its own `role: "tool"` message.
                    messages.push(WireMessage {
                        role: "tool",
                        content: Some(content.clone()),
                        tool_calls: vec![],
                        tool_call_id: Some(tool_use_id.clone()),
                    });
                }
            }
        }

        if !text_parts.is_empty() || !tool_calls.is_empty() {
            messages.push(WireMessage {
                role,
                content: if text_parts.is_empty() {
                    None
                } else {
                    Some(text_parts.join(""))
                },
                tool_calls,
                tool_call_id: None,
            });
        }
    }

    let tools = req
        .tools
        .iter()
        .map(|t| WireTool {
            r#type: "function",
            function: WireFunctionSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect();

    WireRequest {
        model: model.to_string(),
        messages,
        tools,
        max_tokens: req.max_tokens,
    }
}

#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireRespMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireRespMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireRespToolCall>,
}

#[derive(Deserialize)]
struct WireRespToolCall {
    id: String,
    function: WireRespFunctionCall,
}

#[derive(Deserialize)]
struct WireRespFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptTokensDetails>,
}

/// OpenAI-shaped caching is automatic/transparent server-side (unlike
/// Anthropic's explicit cache-control blocks), so only a read count is ever
/// reported here -- there's no equivalent "cache write" figure to parse.
#[derive(Deserialize)]
struct WirePromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

fn from_wire_response(wire: WireResponse) -> ModelResponse {
    let choice = wire.choices.into_iter().next();
    let (content, finish_reason) = match choice {
        Some(c) => (c.message, c.finish_reason),
        None => (
            WireRespMessage {
                content: None,
                tool_calls: vec![],
            },
            None,
        ),
    };

    let mut blocks = Vec::new();
    if let Some(text) = content.content {
        if !text.is_empty() {
            blocks.push(ChatContent::Text(text));
        }
    }
    for tc in content.tool_calls {
        let input = serde_json::from_str(&tc.function.arguments)
            .unwrap_or(serde_json::Value::String(tc.function.arguments));
        blocks.push(ChatContent::ToolUse {
            id: tc.id,
            name: tc.function.name,
            input,
        });
    }

    let stop_reason = match finish_reason.as_deref() {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    ModelResponse {
        content: blocks,
        stop_reason,
        usage: wire
            .usage
            .map(|u| ModelUsage {
                input_tokens: Some(u.prompt_tokens),
                output_tokens: Some(u.completion_tokens),
                cache_read_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens),
                cache_write_tokens: None,
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ToolSpec};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sends_the_expected_wire_shape_and_parses_a_text_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "hello from openai-compatible"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 6}
            })))
            .mount(&server)
            .await;

        let provider = OpenAiCompatibleProvider::new(
            server.uri(),
            "test-key",
            "local-model",
            HashMap::new(),
        );

        let resp = provider
            .complete(ModelRequest {
                model: "local-model".to_string(),
                system: Some("be terse".to_string()),
                messages: vec![ChatMessage::user_text("hi")],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap();

        assert_eq!(resp.text(), "hello from openai-compatible");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, Some(12));
        assert_eq!(resp.usage.output_tokens, Some(6));
    }

    #[tokio::test]
    async fn parses_cache_read_tokens_but_never_reports_cache_writes() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 6,
                    "prompt_tokens_details": {"cached_tokens": 8}
                }
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleProvider::new(server.uri(), "test-key", "local-model", HashMap::new());

        let resp = provider
            .complete(ModelRequest {
                model: "local-model".to_string(),
                system: None,
                messages: vec![ChatMessage::user_text("hi")],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap();

        assert_eq!(resp.usage.cache_read_tokens, Some(8));
        assert_eq!(resp.usage.cache_write_tokens, None);
    }

    #[tokio::test]
    async fn cache_read_tokens_is_none_when_the_wire_response_omits_prompt_tokens_details() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 6}
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleProvider::new(server.uri(), "test-key", "local-model", HashMap::new());

        let resp = provider
            .complete(ModelRequest {
                model: "local-model".to_string(),
                system: None,
                messages: vec![ChatMessage::user_text("hi")],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap();

        assert_eq!(resp.usage.cache_read_tokens, None);
        assert_eq!(resp.usage.cache_write_tokens, None);
    }

    #[tokio::test]
    async fn parses_a_tool_call_response_with_stringified_arguments() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 8}
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleProvider::new(server.uri(), "test-key", "local-model", HashMap::new());

        let resp = provider
            .complete(ModelRequest {
                model: "local-model".to_string(),
                system: None,
                messages: vec![ChatMessage::user_text("read the readme")],
                tools: vec![ToolSpec {
                    name: "read_file".to_string(),
                    description: "reads a file".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
                max_tokens: 100,
            })
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        let uses = resp.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].1, "read_file");
        assert_eq!(uses[0].2["path"], "README.md");
    }

    #[test]
    fn tool_results_become_separate_role_tool_messages() {
        let req = ModelRequest {
            model: "local-model".to_string(),
            system: Some("sys".to_string()),
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "a.txt"}),
                    }],
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatContent::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "file contents".to_string(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
        };

        let wire = to_wire_request(&req, "local-model");
        // system, assistant-with-tool_calls, tool
        assert_eq!(wire.messages.len(), 3);
        assert_eq!(wire.messages[0].role, "system");
        assert_eq!(wire.messages[1].role, "assistant");
        assert_eq!(wire.messages[1].tool_calls.len(), 1);
        assert_eq!(wire.messages[1].tool_calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(wire.messages[2].role, "tool");
        assert_eq!(wire.messages[2].tool_call_id.as_deref(), Some("call_1"));
    }
}
