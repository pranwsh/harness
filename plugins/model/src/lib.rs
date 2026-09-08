use std::sync::Arc;

use harness_contracts::{
    CH_LLM_REQUEST_HEADERS, CH_LLM_RESPONSE_HEADERS, KEY_CONFIG, KEY_MODEL_CLIENT,
    LlmRequestHeaders, LlmResponseHeaders, Message, Role, ToolCall, ToolSpec,
};
use harness_core::{Context, Plugin, PluginMeta};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use harness_config::AppConfig;

/// Boxed completion future returned by [`ModelClient`].
pub type CompletionFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Result<Message, ModelError>> + Send + 'a>>;

/// Client abstraction over a chat-completion backend.
///
/// Dyn-compatible: implementations are registered as `Arc<dyn ModelClient>`
/// services and may be swapped (HTTP client, fake for tests) without
/// touching the loop.
pub trait ModelClient: Send + Sync + 'static {
    /// Runs one non-streaming completion.
    ///
    /// `tools` advertises callable tools; the returned message may carry
    /// `tool_calls` the caller is expected to execute and feed back.
    fn complete<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> CompletionFuture<'a>;
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider error {status}: {body}")]
    Provider {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("provider returned no choices")]
    EmptyChoices,
    #[error("provider returned no message content")]
    EmptyContent,
    #[error("request denied: {0}")]
    Denied(String),
}

/// OpenAI-compatible `/chat/completions` client.
///
/// Sends through the `llm.request_headers` waterfall before each request so
/// an optional headers plugin can inject/validate headers (or veto the
/// send), and emits `llm.response_headers` after each response for
/// observation. With no listeners both hooks are no-ops.
pub struct HttpModelClient {
    ctx: Context,
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpModelClient {
    pub fn new(ctx: Context, config: &AppConfig) -> Result<Self, ModelError> {
        // No `User-Agent` header goes out at all when unconfigured; reqwest
        // only sends one when the builder flag is set.
        let mut builder = reqwest::Client::builder();
        if !config.llm.user_agent.trim().is_empty() {
            builder = builder.user_agent(&config.llm.user_agent);
        }
        let client = builder.build()?;
        Ok(HttpModelClient {
            ctx,
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

/// Applies one `(name, value)` header pair, skipping entries that fail
/// client-side parsing instead of failing the whole request.
fn apply_header(
    request: reqwest::RequestBuilder,
    name: &str,
    value: &str,
) -> reqwest::RequestBuilder {
    if !valid_header(name, value) {
        return request;
    }
    request.header(name, value)
}

/// True when both sides parse as a valid HTTP header pair.
fn valid_header(name: &str, value: &str) -> bool {
    use std::str::FromStr;
    reqwest::header::HeaderName::from_str(name).is_ok()
        && reqwest::header::HeaderValue::from_str(value).is_ok()
}

impl ModelClient for HttpModelClient {
    fn complete<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> CompletionFuture<'a> {
        Box::pin(async move {
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
            // Pre hook: optional headers plugins may inject/validate headers
            // or veto the send. No handlers = send as seeded. Fail-closed on
            // waterfall infra errors so a broken hook can't be bypassed.
            let seed = vec![(
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            )];
            let hooked = match self
                .ctx
                .waterfall_key(
                    CH_LLM_REQUEST_HEADERS,
                    LlmRequestHeaders::allow(model, seed),
                )
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    return Err(ModelError::Denied(format!("request hook failed: {e}")));
                }
            };
            if let Some(reason) = hooked.denied.as_deref().map(str::trim) {
                return Err(ModelError::Denied(if reason.is_empty() {
                    "denied by headers hook".to_owned()
                } else {
                    format!("denied by headers hook: {reason}")
                }));
            }
            let mut request = self.client.post(url);
            for (name, value) in &hooked.headers {
                request = apply_header(request, name, value);
            }
            let resp = request.json(&body).send().await?;
            let status = resp.status();
            // Post hook (observation only): fire-and-forget so slow
            // listeners can never stall the turn.
            let _ = self.ctx.emit_key_detached(
                CH_LLM_RESPONSE_HEADERS,
                LlmResponseHeaders {
                    model: model.to_owned(),
                    status: status.as_u16(),
                    headers: resp
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.to_string(),
                                value.to_str().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect(),
                },
            );
            if !status.is_success() {
                // Provider error bodies carry the actual reason (unknown
                // model, bad field, auth gating, …); keep a prefix so the
                // message stays one line in the TUI.
                let raw = resp.text().await.unwrap_or_default();
                let body: String = raw.chars().take(300).collect();
                return Err(ModelError::Provider { status, body });
            }
            let resp: CompletionResponse = resp.json().await?;

            let choice = resp
                .choices
                .into_iter()
                .next()
                .ok_or(ModelError::EmptyChoices)?;
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
        })
    }
}

/// Sized handle around a dyn client so it can live in the service registry
/// (`inject_key` requires `Sized`).
pub struct ModelClientHandle(pub Arc<dyn ModelClient>);

impl ModelClient for ModelClientHandle {
    fn complete<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> CompletionFuture<'a> {
        self.0.complete(model, messages, tools)
    }
}

pub struct ModelPlugin;

impl Plugin for ModelPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("model")
            .provides(KEY_MODEL_CLIENT)
            .injects(KEY_CONFIG)
            .waterfalls::<LlmRequestHeaders>(CH_LLM_REQUEST_HEADERS)
            .emits::<LlmResponseHeaders>(CH_LLM_RESPONSE_HEADERS)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;
        let client = HttpModelClient::new(ctx.clone(), &config)
            .map_err(|e| harness_core::Error::PluginPanicked("model".to_owned(), e.to_string()))?;
        ctx.provide_key(
            KEY_MODEL_CLIENT,
            Arc::new(ModelClientHandle(Arc::new(client))),
        );
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

    #[test]
    fn provider_error_reports_status_and_body() {
        let err = ModelError::Provider {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: r#"{"error":{"type":"MissingSessionID"}}"#.to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("400"), "status: {msg:?}");
        assert!(msg.contains("MissingSessionID"), "body: {msg:?}");
    }

    #[test]
    fn invalid_header_pairs_are_rejected() {
        assert!(valid_header("x-title", "agent"));
        assert!(valid_header("authorization", "Bearer k"));
        assert!(!valid_header("not a name", "v"));
        assert!(!valid_header("x-ok", "bad\nvalue"));
        assert!(!valid_header("", "v"));
    }

    fn test_client(ctx: harness_core::Context) -> HttpModelClient {
        let config = AppConfig::from_toml(
            "[llm]\nbase_url = \"http://127.0.0.1:9\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n",
            "test",
        )
        .unwrap();
        HttpModelClient::new(ctx, &config).unwrap()
    }

    #[test]
    fn client_builds_without_user_agent() {
        // Absent/empty UA must not fail construction; no `User-Agent`
        // header goes out in that case.
        for raw in [
            "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\n",
            "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"  \"\n",
        ] {
            let config = AppConfig::from_toml(raw, "test").unwrap();
            assert_eq!(config.llm.user_agent.trim(), "");
            let _ = HttpModelClient::new(harness_core::Context::root(), &config).unwrap();
        }
    }

    #[tokio::test]
    async fn denied_request_never_sends() {
        use harness_contracts::{CH_LLM_REQUEST_HEADERS, LlmRequestHeaders};

        let ctx = harness_core::Context::root();
        ctx.on_waterfall_key::<LlmRequestHeaders, _, _>(CH_LLM_REQUEST_HEADERS, |req| async move {
            LlmRequestHeaders::deny(req.model.clone(), req.headers.clone(), "nope")
        })
        .unwrap();
        // Unroutable base URL proves nothing is sent: the veto fires first.
        let client = test_client(ctx);
        let err = client.complete("m", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ModelError::Denied(_)), "got: {err:?}");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn waterfall_rewrite_reaches_send() {
        use harness_contracts::{CH_LLM_REQUEST_HEADERS, LlmRequestHeaders};

        let ctx = harness_core::Context::root();
        ctx.on_waterfall_key::<LlmRequestHeaders, _, _>(CH_LLM_REQUEST_HEADERS, |req| async move {
            let mut next = (*req).clone();
            // Invalid entry must be skipped, not fail the request.
            next.headers.push(("not a name".to_owned(), "v".to_owned()));
            next
        })
        .unwrap();
        // Unroutable: any send attempt surfaces as Http, proving the
        // waterfall ran and only the veto path short-circuits.
        let client = test_client(ctx);
        let err = client.complete("m", &[], &[]).await.unwrap_err();
        assert!(matches!(err, ModelError::Http(_)), "got: {err:?}");
    }
}
