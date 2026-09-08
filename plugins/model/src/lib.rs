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

/// One item of a streaming completion.
///
/// The loop forwards `Content` slices for immediate display and resolves the
/// turn from the terminal event; `Failed` preserves already-shown text and
/// surfaces through the normal turn-failure path.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A slice of assistant text, to display immediately.
    Content(String),
    /// The stream ended; the fully assembled message (content + tool calls).
    Done(Message),
    /// The stream failed after `Content` may already have been emitted.
    Failed(String),
}

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

    /// Runs a streaming completion, yielding text slices as they arrive.
    ///
    /// Default: replays [`ModelClient::complete`] as a single chunk, so
    /// fakes and third-party impls behave (one `Content` plus `Done`)
    /// without implementing SSE themselves. The by-value `Arc` receiver
    /// keeps the method object-safe while letting the replay task own the
    /// client (call as `Arc::clone(&client).stream(…)`).
    fn stream(
        self: Arc<Self>,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> tokio::sync::mpsc::Receiver<StreamEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let model = model.to_owned();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        tokio::spawn(async move {
            match self.complete(&model, &messages, &tools).await {
                Ok(msg) => {
                    if !msg.content.is_empty() {
                        let _ = tx.send(StreamEvent::Content(msg.content.clone())).await;
                    }
                    let _ = tx.send(StreamEvent::Done(msg)).await;
                }
                Err(err) => {
                    let _ = tx.send(StreamEvent::Failed(err.to_string())).await;
                }
            }
        });
        rx
    }
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

    /// Pre hook shared by both modes: optional headers plugins may
    /// inject/validate headers or veto the send. No handlers = send as
    /// seeded. Fail-closed on waterfall infra errors so a broken hook can't
    /// be bypassed.
    async fn gate_request(&self, model: &str) -> Result<LlmRequestHeaders, ModelError> {
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
        Ok(hooked)
    }

    /// Post hook shared by both modes (observation only): fire-and-forget
    /// so slow listeners can never stall the turn.
    fn observe_response(
        &self,
        model: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) {
        let _ = self.ctx.emit_key_detached(
            CH_LLM_RESPONSE_HEADERS,
            LlmResponseHeaders {
                model: model.to_owned(),
                status: status.as_u16(),
                headers: headers
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
    }

    /// Applies the hooked headers to a request builder, skipping entries
    /// that fail client-side parsing instead of failing the request.
    fn with_headers(
        &self,
        mut request: reqwest::RequestBuilder,
        headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        for (name, value) in headers {
            request = apply_header(request, name, value);
        }
        request
    }

    /// Streams one completion, forwarding text slices immediately and
    /// resolving to the fully assembled message. Runs on a spawned task so
    /// `stream` can hand back the receiver synchronously.
    async fn run_stream(
        self: Arc<Self>,
        model: String,
        messages: Vec<Message>,
        tools: Vec<ToolSpec>,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) {
        let hooked = match self.gate_request(&model).await {
            Ok(hooked) => hooked,
            Err(err) => {
                let _ = tx.send(StreamEvent::Failed(err.to_string())).await;
                return;
            }
        };
        let url = format!("{}/chat/completions", self.base_url);
        let body = completion_body(&model, &messages, &tools, true);
        let resp = match self
            .with_headers(self.client.post(url), &hooked.headers)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                let _ = tx.send(StreamEvent::Failed(err.to_string())).await;
                return;
            }
        };
        let status = resp.status();
        self.observe_response(&model, status, resp.headers());
        if !status.is_success() {
            // Same one-line provider error shape as non-streaming mode.
            let raw = resp.text().await.unwrap_or_default();
            let body: String = raw.chars().take(300).collect();
            let _ = tx
                .send(StreamEvent::Failed(format!(
                    "provider error {status}: {body}"
                )))
                .await;
            return;
        }
        let mut accum = StreamAccum::default();
        let mut buffer = Vec::new();
        let mut byte_stream = resp.bytes_stream();
        use futures::StreamExt;
        loop {
            let Some(chunk) = byte_stream.next().await else {
                break;
            };
            let Ok(bytes) = chunk else {
                let _ = tx
                    .send(StreamEvent::Failed("stream interrupted".to_owned()))
                    .await;
                return;
            };
            let (slices, finished) = fold_bytes(&mut buffer, &bytes, &mut accum);
            for text in slices {
                let _ = tx.send(StreamEvent::Content(text)).await;
            }
            if finished {
                break;
            }
        }
        if !buffer.iter().all(|b| b.is_ascii_whitespace()) {
            let tail = std::mem::take(&mut buffer);
            // A stream may end on `data: [DONE]` with no trailing newline;
            // assembling below covers either outcome, so only text matters.
            if let LineOutcome::Content(text) =
                handle_sse_line(&String::from_utf8_lossy(&tail), &mut accum)
            {
                let _ = tx.send(StreamEvent::Content(text)).await;
            }
        }
        match assemble_stream(accum) {
            Some(msg) => {
                let _ = tx.send(StreamEvent::Done(msg)).await;
            }
            None => {
                let _ = tx
                    .send(StreamEvent::Failed(
                        "provider returned no message content".to_owned(),
                    ))
                    .await;
            }
        }
    }
}

#[derive(Serialize)]
struct CompletionRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Builds the shared chat-completions body for both modes; `stream: false`
/// stays absent from the wire format so non-streaming requests are unchanged.
fn completion_body<'a>(
    model: &'a str,
    messages: &'a [Message],
    tools: &'a [ToolSpec],
    stream: bool,
) -> CompletionRequest<'a> {
    CompletionRequest {
        model,
        messages: messages.iter().map(WireMessage::from_message).collect(),
        tools: tools
            .iter()
            .map(|t| WireTool {
                r#type: "function",
                function: t,
            })
            .collect(),
        stream,
    }
}

/// One Server-Sent Events data payload of an OpenAI-compatible streaming
/// response. Unknown fields (usage, reasoning deltas, logprobs, …) are
/// ignored.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Incremental assembly of one streamed assistant message: text plus
/// tool-call fragments keyed by their stream `index`.
#[derive(Debug, Default)]
struct StreamAccum {
    content: String,
    calls: Vec<PendingToolCall>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Outcome of feeding one SSE line to the accumulator.
#[derive(Debug, PartialEq, Eq)]
enum LineOutcome {
    /// Blank line, keep-alive comment, foreign event, or malformed JSON.
    Ignored,
    /// A text slice to display immediately.
    Content(String),
    /// The `[DONE]` terminator.
    Finished,
}

/// Folds one raw SSE line into `accum`. Pure: unit-tested with canned
/// input, including lines the caller split across TCP segments.
fn handle_sse_line(line: &str, accum: &mut StreamAccum) -> LineOutcome {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return LineOutcome::Ignored;
    }
    let Some(payload) = line.strip_prefix("data:") else {
        return LineOutcome::Ignored;
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return LineOutcome::Finished;
    }
    let Ok(chunk) = serde_json::from_str::<StreamChunk>(payload) else {
        return LineOutcome::Ignored;
    };
    let Some(choice) = chunk.choices.into_iter().next() else {
        return LineOutcome::Ignored;
    };
    if let Some(text) = choice.delta.content
        && !text.is_empty()
    {
        accum.content.push_str(&text);
        return LineOutcome::Content(text);
    }
    for fragment in choice.delta.tool_calls {
        if fragment.index >= accum.calls.len() {
            accum
                .calls
                .resize_with(fragment.index + 1, PendingToolCall::default);
        }
        let slot = &mut accum.calls[fragment.index];
        if slot.id.is_none() {
            slot.id = fragment.id;
        }
        if slot.name.is_none() {
            slot.name = fragment.function.as_ref().and_then(|f| f.name.clone());
        }
        if let Some(args) = fragment.function.as_ref().and_then(|f| f.arguments.clone()) {
            slot.arguments.push_str(&args);
        }
    }
    LineOutcome::Ignored
}

/// Folds newly arrived bytes: decodes complete `\n`-terminated lines (kept
/// as raw bytes until the newline so split multibyte characters survive)
/// and feeds each to the accumulator. Returns emitted text slices and
/// whether `[DONE]` arrived.
fn fold_bytes(buffer: &mut Vec<u8>, bytes: &[u8], accum: &mut StreamAccum) -> (Vec<String>, bool) {
    buffer.extend_from_slice(bytes);
    let mut out = Vec::new();
    let mut finished = false;
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=pos).collect();
        match handle_sse_line(&String::from_utf8_lossy(&raw), accum) {
            LineOutcome::Content(text) => out.push(text),
            LineOutcome::Finished => {
                finished = true;
                break;
            }
            LineOutcome::Ignored => {}
        }
    }
    (out, finished)
}

/// Builds the final message, or `None` when the stream carried nothing
/// (mirrors the `EmptyContent` error of non-streaming completions).
fn assemble_stream(accum: StreamAccum) -> Option<Message> {
    if accum.content.is_empty() && accum.calls.is_empty() {
        return None;
    }
    let calls = accum
        .calls
        .into_iter()
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: call.id.unwrap_or_else(|| format!("stream-call-{index}")),
            name: call.name.unwrap_or_default(),
            arguments: call.arguments,
        })
        .collect();
    Some(Message::assistant_with_calls(accum.content, calls))
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
            let body = completion_body(model, messages, tools, false);
            let hooked = self.gate_request(model).await?;
            let resp = self
                .with_headers(self.client.post(url), &hooked.headers)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            self.observe_response(model, status, resp.headers());
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

    fn stream(
        self: Arc<Self>,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> tokio::sync::mpsc::Receiver<StreamEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let model = model.to_owned();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        tokio::spawn(async move {
            self.run_stream(model, messages, tools, tx).await;
        });
        rx
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

    fn stream(
        self: Arc<Self>,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> tokio::sync::mpsc::Receiver<StreamEvent> {
        Arc::clone(&self.0).stream(model, messages, tools)
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
            stream: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("tools").is_none());
        assert!(json.get("stream").is_none(), "non-streaming body unchanged");
    }

    #[test]
    fn request_body_marks_streaming() {
        let msg = Message::user("hi");
        let body = completion_body("m", std::slice::from_ref(&msg), &[], true);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["stream"], true);
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

    #[test]
    fn sse_lines_fold_content_and_finish() {
        let mut accum = StreamAccum::default();
        let out = handle_sse_line(
            r#"data: {"choices":[{"delta":{"content":"hel"}}]}"#,
            &mut accum,
        );
        assert_eq!(out, LineOutcome::Content("hel".to_owned()));
        assert_eq!(accum.content, "hel");
        assert_eq!(
            handle_sse_line("", &mut accum),
            LineOutcome::Ignored,
            "blank line"
        );
        assert_eq!(
            handle_sse_line(": keep-alive", &mut accum),
            LineOutcome::Ignored,
            "comment"
        );
        assert_eq!(
            handle_sse_line("event: message", &mut accum),
            LineOutcome::Ignored,
            "foreign event"
        );
        assert_eq!(
            handle_sse_line("data: not json", &mut accum),
            LineOutcome::Ignored,
            "malformed"
        );
        assert_eq!(
            handle_sse_line(r#"data: {"choices":[]}"#, &mut accum),
            LineOutcome::Ignored,
            "no choice"
        );
        assert_eq!(
            handle_sse_line("data: [DONE]", &mut accum),
            LineOutcome::Finished
        );
    }

    #[test]
    fn sse_tool_fragments_accumulate_by_index() {
        let mut accum = StreamAccum::default();
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"edit","arguments":"{}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
        ] {
            assert_eq!(handle_sse_line(line, &mut accum), LineOutcome::Ignored);
        }
        let msg = assemble_stream(accum).expect("assembled");
        assert_eq!(msg.content, "");
        assert_eq!(msg.tool_calls.len(), 2);
        assert_eq!(msg.tool_calls[0].id, "c1");
        assert_eq!(msg.tool_calls[0].name, "read");
        assert_eq!(msg.tool_calls[0].arguments, "{\"path\":\"x\"}");
        assert_eq!(msg.tool_calls[1].id, "c2");
    }

    #[test]
    fn assemble_stream_needs_some_signal() {
        assert!(assemble_stream(StreamAccum::default()).is_none());
        let mut accum = StreamAccum::default();
        accum.content.push_str("hi");
        let msg = assemble_stream(accum).expect("content suffices");
        assert_eq!(msg.content, "hi");
        assert!(msg.tool_calls.is_empty());
    }

    #[test]
    fn fold_bytes_survives_segment_splits() {
        let mut accum = StreamAccum::default();
        let mut buffer = Vec::new();
        let line1 = "data: {\"choices\":[{\"delta\":{\"content\":\"hé\"}}]}\n";
        let line2 = "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n";
        let done = "data: [DONE]\n";
        let cut = |s: &str, at: usize| (s.as_bytes()[..at].to_vec(), s.as_bytes()[at..].to_vec());
        // Cut line1 inside the two-byte `é`.
        let e_off = line1
            .as_bytes()
            .windows(2)
            .position(|w| w == "é".as_bytes())
            .unwrap();
        let (a, b) = cut(line1, e_off + 1);
        // Cut line2 mid-line and glue the terminator to its tail.
        let (c, d) = cut(line2, 10);
        let mut slices = Vec::new();
        let mut finished = false;
        for piece in [a, b, c, [d, done.as_bytes().to_vec()].concat()] {
            let (mut out, done) = fold_bytes(&mut buffer, &piece, &mut accum);
            slices.append(&mut out);
            finished = finished || done;
        }
        assert!(finished, "terminator seen");
        assert_eq!(slices, vec!["hé", "llo"]);
        assert_eq!(accum.content, "héllo");
        assert!(buffer.iter().all(|b| b.is_ascii_whitespace()));
    }

    /// Fake implementing only `complete`: exercises the default `stream`
    /// replay used by fakes and third-party clients.
    struct ScriptedModel {
        reply: Message,
    }

    impl ModelClient for ScriptedModel {
        fn complete<'a>(
            &'a self,
            _model: &'a str,
            _messages: &'a [Message],
            _tools: &'a [ToolSpec],
        ) -> CompletionFuture<'a> {
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }
    }

    async fn collect_stream(rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, StreamEvent::Done(_) | StreamEvent::Failed(_));
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn default_stream_replays_complete_as_one_chunk() {
        let model = Arc::new(ScriptedModel {
            reply: Message::assistant("hello there"),
        });
        let mut rx = model.stream("m", &[], &[]);
        let events = collect_stream(&mut rx).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Content(c) if c == "hello there"));
        assert!(matches!(&events[1], StreamEvent::Done(m) if m.content == "hello there"));
    }

    #[tokio::test]
    async fn default_stream_maps_errors_to_failed() {
        struct BrokenModel;
        impl ModelClient for BrokenModel {
            fn complete<'a>(
                &'a self,
                _model: &'a str,
                _messages: &'a [Message],
                _tools: &'a [ToolSpec],
            ) -> CompletionFuture<'a> {
                Box::pin(async move { Err(ModelError::EmptyChoices) })
            }
        }
        let model = Arc::new(BrokenModel);
        let mut rx = model.stream("m", &[], &[]);
        let events = collect_stream(&mut rx).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Failed(_)));
    }

    #[tokio::test]
    async fn stream_veto_fails_without_sending() {
        use harness_contracts::{CH_LLM_REQUEST_HEADERS, LlmRequestHeaders};

        let ctx = harness_core::Context::root();
        ctx.on_waterfall_key::<LlmRequestHeaders, _, _>(CH_LLM_REQUEST_HEADERS, |req| async move {
            LlmRequestHeaders::deny(req.model.clone(), req.headers.clone(), "nope")
        })
        .unwrap();
        // Unroutable base URL proves nothing is sent: the veto fires first.
        let client = Arc::new(test_client(ctx));
        let mut rx = client.stream("m", &[], &[]);
        let events = collect_stream(&mut rx).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Failed(err) => assert!(err.contains("nope"), "got: {err}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
