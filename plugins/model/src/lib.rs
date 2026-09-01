use std::sync::Arc;

use harness_contracts::{KEY_CONFIG, KEY_MODEL_CLIENT, Message, Role, ToolCall, ToolSpec};
use harness_core::{Context, Plugin, PluginMeta};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use harness_config::AppConfig;

/// Client abstraction over a chat-completion backend.
///
/// Implementations are expected to be cheap to clone behind an `Arc` and
/// safe to call concurrently.
pub trait ModelClient: Send + Sync + 'static {
    /// Runs one non-streaming completion.
    ///
    /// `tools` advertises callable tools; the returned message may carry
    /// `tool_calls` the caller is expected to execute and feed back.
    fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> impl Future<Output = Result<Message, ModelError>> + Send;
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned no choices")]
    EmptyChoices,
    #[error("provider returned no message content")]
    EmptyContent,
}

/// OpenAI-compatible `/chat/completions` client.
pub struct HttpModelClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpModelClient {
    pub fn new(config: &AppConfig) -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .user_agent(&config.llm.user_agent)
            .build()?;
        Ok(HttpModelClient {
            client,
            base_url: config.llm.base_url.trim_end_matches('/').to_owned(),
            api_key: config.llm.api_key.clone(),
        })
    }
}

#[derive(Serialize)]
struct CompletionRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    r#type: &'a str,
    function: &'a ToolSpec,
}

// Wire format: tool calls use the provider's nested shape.
#[derive(Serialize, Clone)]
struct WireMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize, Clone)]
struct WireToolCall<'a> {
    id: &'a str,
    r#type: &'a str,
    function: WireFunction<'a>,
}

#[derive(Serialize, Clone)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl<'a> WireMessage<'a> {
    fn from_message(msg: &'a Message) -> Self {
        WireMessage {
            role: match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            },
            content: Some(msg.content.as_str()),
            tool_calls: msg
                .tool_calls
                .iter()
                .map(|c| WireToolCall {
                    id: c.id.as_str(),
                    r#type: "function",
                    function: WireFunction {
                        name: c.name.as_str(),
                        arguments: c.arguments.as_str(),
                    },
                })
                .collect(),
            tool_call_id: msg.call_id.as_deref(),
        }
    }
}

#[derive(Deserialize)]
struct CompletionResponse {
    choices: Vec<CompletionChoice>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
}

#[derive(Deserialize)]
struct CompletionMessage {
    content: Option<String>,
    tool_calls: Option<Vec<RespToolCall>>,
}

#[derive(Deserialize)]
struct RespToolCall {
    id: String,
    function: RespFunction,
}

#[derive(Deserialize)]
struct RespFunction {
    name: String,
    arguments: String,
}

impl ModelClient for HttpModelClient {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Message, ModelError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = CompletionRequest {
            model,
            messages: messages.iter().map(WireMessage::from_message).collect(),
            tools: tools
                .iter()
                .map(|t| WireTool {
                    r#type: "function",
                    function: t,
                })
                .collect(),
        };
        let resp: CompletionResponse = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let choice = resp.choices.into_iter().next().ok_or(ModelError::EmptyChoices)?;
        let msg = choice.message;
        let tool_calls: Vec<ToolCall> = msg
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|c| ToolCall {
                id: c.id,
                name: c.function.name,
                arguments: c.function.arguments,
            })
            .collect();

        let content = msg.content.unwrap_or_default();
        if content.is_empty() && tool_calls.is_empty() {
            return Err(ModelError::EmptyContent);
        }
        Ok(Message::assistant_with_calls(content, tool_calls))
    }
}

pub struct ModelPlugin;

impl Plugin for ModelPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("model")
            .provides(KEY_MODEL_CLIENT)
            .injects(KEY_CONFIG)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;
        let client = HttpModelClient::new(&config)
            .map_err(|e| harness_core::Error::PluginPanicked("model".to_owned(), e.to_string()))?;
        ctx.provide_key(KEY_MODEL_CLIENT, Arc::new(client));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_message_serializes_minimal_user_message() {
        let msg = Message::user("hello");
        let wire = WireMessage::from_message(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("tool_call_id").is_none());
    }

    #[test]
    fn wire_message_serializes_tool_calls() {
        let call = harness_contracts::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        };
        let msg = Message::assistant_with_calls("thinking", vec![call]);
        let wire = WireMessage::from_message(&msg);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["tool_calls"][0]["id"], "t1");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(json["tool_calls"][0]["type"], "function");
    }

    #[test]
    fn request_skips_tools_when_empty() {
        let msg = Message::user("hi");
        let req = CompletionRequest {
            model: "m",
            messages: vec![WireMessage::from_message(&msg)],
            tools: Vec::new(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("tools").is_none());
    }
}
