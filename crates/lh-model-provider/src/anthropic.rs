//! Anthropic Messages API protocol implementation (architecture §13.1).

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    ChatContent, ChatRole, ModelProvider, ModelProviderCapabilities, ModelRequest, ModelResponse,
    ModelUsage, ProviderError, Result, StopReason, ToolSpec,
};

pub struct AnthropicProtocolProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    extra_headers: HashMap<String, String>,
}

impl AnthropicProtocolProvider {
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
impl ModelProvider for AnthropicProtocolProvider {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        let wire = to_wire_request(&req, &self.model);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let mut builder = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
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
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<WireContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    Text {
        text: String,
    },
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

#[derive(Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn to_wire_request(req: &ModelRequest, model: &str) -> WireRequest {
    let messages = req
        .messages
        .iter()
        .map(|m| WireMessage {
            role: match m.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            },
            content: m
                .content
                .iter()
                .map(|c| match c {
                    ChatContent::Text(t) => WireContentBlock::Text { text: t.clone() },
                    ChatContent::ToolUse { id, name, input } => WireContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                    ChatContent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => WireContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                    },
                })
                .collect(),
        })
        .collect();

    let tools = req
        .tools
        .iter()
        .map(|t: &ToolSpec| WireTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect();

    WireRequest {
        model: model.to_string(),
        max_tokens: req.max_tokens,
        system: req.system.clone(),
        messages,
        tools,
    }
}

#[derive(Deserialize)]
struct WireResponse {
    content: Vec<WireRespBlock>,
    stop_reason: Option<String>,
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireRespBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
}

fn from_wire_response(wire: WireResponse) -> ModelResponse {
    let content = wire
        .content
        .into_iter()
        .filter_map(|b| match b {
            WireRespBlock::Text { text } => Some(ChatContent::Text(text)),
            WireRespBlock::ToolUse { id, name, input } => {
                Some(ChatContent::ToolUse { id, name, input })
            }
            WireRespBlock::Unknown => None,
        })
        .collect();

    let stop_reason = match wire.stop_reason.as_deref() {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    ModelResponse {
        content,
        stop_reason,
        usage: ModelUsage {
            input_tokens: Some(wire.usage.input_tokens),
            output_tokens: Some(wire.usage.output_tokens),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sends_the_expected_wire_shape_and_parses_a_text_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "hello from anthropic"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let provider = AnthropicProtocolProvider::new(
            server.uri(),
            "test-key",
            "claude-test",
            HashMap::new(),
        );

        let resp = provider
            .complete(ModelRequest {
                model: "claude-test".to_string(),
                system: Some("be terse".to_string()),
                messages: vec![ChatMessage::user_text("hi")],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap();

        assert_eq!(resp.text(), "hello from anthropic");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, Some(10));
        assert_eq!(resp.usage.output_tokens, Some(5));
    }

    #[tokio::test]
    async fn parses_a_tool_use_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [
                    {"type": "text", "text": "let me check that file"},
                    {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "README.md"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 20, "output_tokens": 8}
            })))
            .mount(&server)
            .await;

        let provider =
            AnthropicProtocolProvider::new(server.uri(), "test-key", "claude-test", HashMap::new());

        let resp = provider
            .complete(ModelRequest {
                model: "claude-test".to_string(),
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
    }

    #[tokio::test]
    async fn surfaces_non_2xx_responses_as_api_errors() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider =
            AnthropicProtocolProvider::new(server.uri(), "bad-key", "claude-test", HashMap::new());

        let err = provider
            .complete(ModelRequest {
                model: "claude-test".to_string(),
                system: None,
                messages: vec![ChatMessage::user_text("hi")],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap_err();

        match err {
            ProviderError::Api { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
