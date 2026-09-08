//! OpenAI Responses transport (`POST {base_url}/responses`).
//!
//! Some Zen models (the `muse-spark-` family) are served on the Responses
//! API only: the same call against `/chat/completions` answers
//! `500 Internal server error`, while `/responses` succeeds. This module
//! owns everything endpoint-specific behind a [`Transport`] selector so
//! future endpoint migrations stay contained here:
//! - [`transport_for`]: model id (+ config extras) → [`Transport`].
//! - Request bodies ([`ResponsesRequest`], [`responses_input`]).
//! - Non-streaming output parsing ([`assemble_responses_output`]).
//! - Streaming SSE folding ([`fold_responses_bytes`]) into text slices.
//!
//! The caller (`HttpModelClient`) keeps reusing the shared header
//! waterfall, response observation, and `StreamEvent` channel, so the
//! agent loop never knows which endpoint served a turn.

use harness_contracts::{Message, ToolCall, ToolSpec};
use serde::{Deserialize, Serialize};

/// Which wire protocol serves a model. Chat completions is the default;
/// responses-only models are listed by [`transport_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    ChatCompletions,
    Responses,
}

/// Built-in Responses-only family: Zen serves every `muse-spark-*` id on
/// `/responses` only (`/chat/completions` answers 500 for them). Acts as
/// the wildcard row of the routing table; individual future ids that need
/// Responses go through `extra_responses_models` instead of code changes.
const RESPONSES_FAMILY_PREFIX: &str = "muse-spark-";

/// Selects the transport for a model id. Explicit config extras
/// (`[llm.responses_models]`) and the built-in family prefix route to
/// Responses; everything else stays on chat completions. Pure and
/// unit-tested.
pub fn transport_for(model: &str, extra_responses_models: &[String]) -> Transport {
    if model.starts_with(RESPONSES_FAMILY_PREFIX)
        || extra_responses_models.iter().any(|m| m == model)
    {
        Transport::Responses
    } else {
        Transport::ChatCompletions
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Shared Responses request body for both modes; `stream: false` stays
/// absent from the wire format so non-streaming requests are minimal.
#[derive(Serialize)]
pub struct ResponsesRequest<'a> {
    pub model: &'a str,
    pub input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool<'a>>,
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
}

#[derive(Serialize)]
pub struct ResponsesTool<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub description: &'a str,
    pub parameters: &'a serde_json::Value,
}

pub fn responses_tools(tools: &[ToolSpec]) -> Vec<ResponsesTool<'_>> {
    tools
        .iter()
        .map(|t| ResponsesTool {
            kind: "function",
            name: t.name.as_str(),
            description: t.description.as_str(),
            parameters: &t.parameters,
        })
        .collect()
}

/// Builds the shared Responses body; mirrors `completion_body` in shape.
pub fn responses_body<'a>(
    model: &'a str,
    messages: &'a [Message],
    tools: &'a [ToolSpec],
    stream: bool,
) -> ResponsesRequest<'a> {
    ResponsesRequest {
        model,
        input: responses_input(messages),
        tools: responses_tools(tools),
        stream,
    }
}

/// Converts chat messages to Responses `input` items. Tool history round
/// trips through `function_call` / `function_call_output` items so
/// multi-iteration agent turns keep working across transports.
pub fn responses_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            harness_contracts::Role::System => {
                out.push(serde_json::json!({
                    "role": "system",
                    "content": [{"type": "input_text", "text": msg.content}],
                }));
            }
            harness_contracts::Role::User => {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": msg.content}],
                }));
            }
            harness_contracts::Role::Assistant => {
                if !msg.content.is_empty() {
                    out.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": msg.content}],
                    }));
                }
                for call in &msg.tool_calls {
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "name": call.name,
                        "arguments": call.arguments,
                        "call_id": call.id,
                    }));
                }
            }
            harness_contracts::Role::Tool => {
                out.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": msg.call_id.clone().unwrap_or_default(),
                    "output": msg.content,
                }));
            }
        }
    }
    out
}

/// One item of a non-streaming Responses `output` array. `reasoning` and
/// other opaque items are carried but ignored at assembly.
#[derive(Debug, Deserialize)]
pub struct ResponsesOutputItem {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: Vec<ResponsesContentPart>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    #[serde(default)]
    pub output: Vec<ResponsesOutputItem>,
}

/// Builds the final message from non-streaming `output` items, or `None`
/// when they carried nothing (mirrors the `EmptyContent` error of chat
/// completions).
pub fn assemble_responses_output(items: Vec<ResponsesOutputItem>) -> Option<Message> {
    let mut content = String::new();
    let mut calls = Vec::new();
    for item in items {
        match item.kind.as_str() {
            "message" => {
                for part in item.content {
                    if part.kind == "output_text"
                        && let Some(text) = part.text
                    {
                        content.push_str(&text);
                    }
                }
            }
            "function_call" => {
                calls.push(ToolCall {
                    id: item
                        .call_id
                        .or(item.id)
                        .unwrap_or_else(|| format!("responses-call-{}", calls.len())),
                    name: item.name.unwrap_or_default(),
                    arguments: item.arguments.unwrap_or_default(),
                });
            }
            // `reasoning` (often encrypted/opaque) and friends: nothing to
            // display, safe to skip.
            _ => {}
        }
    }
    if content.is_empty() && calls.is_empty() {
        return None;
    }
    Some(Message::assistant_with_calls(content, calls))
}

/// Incremental assembly of one streamed Responses turn: text plus
/// function-call fragments keyed by their stream `item_id`.
#[derive(Debug, Default)]
pub struct ResponsesAccum {
    pub content: String,
    /// Completed message texts, kept only as a fallback when a stream
    /// carries no text deltas (deltas are the primary source).
    pub done_texts: Vec<String>,
    pub calls: Vec<ResponsesPendingCall>,
}

#[derive(Debug, Default)]
pub struct ResponsesPendingCall {
    pub item_id: String,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

/// Outcome of feeding one Responses SSE event to the accumulator.
#[derive(Debug, PartialEq, Eq)]
pub enum ResponsesLineOutcome {
    /// Unknown event, keep-alive, malformed JSON, or a state-only event.
    Ignored,
    /// A text slice to display immediately.
    Content(String),
    /// The `response.completed` / `response.incomplete` terminator.
    Finished,
    /// The `response.failed` terminator, carrying a displayable reason.
    Failed(String),
}

/// Folds one Responses SSE event into `accum`. Pure: unit-tested with
/// canned input. `event` is the `event:` line value (or the payload's
/// `type` field when the server omits `event:` lines); `data` is the
/// `data:` line value.
pub fn handle_responses_event(
    event: &str,
    data: &str,
    accum: &mut ResponsesAccum,
) -> ResponsesLineOutcome {
    use ResponsesLineOutcome::*;
    let data = data.trim();
    if data.is_empty() {
        return Ignored;
    }
    if data == "[DONE]" {
        return Finished;
    }
    // Tolerate servers that omit `event:` lines: every Responses payload
    // carries its own `type`.
    let event = if event.is_empty() {
        serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|v| v.get("type")?.as_str().map(str::to_owned))
            .unwrap_or_default()
    } else {
        event.to_owned()
    };
    match event.as_str() {
        "response.output_text.delta" => {
            let delta = parse_str_field(data, "delta");
            if delta.is_empty() {
                Ignored
            } else {
                accum.content.push_str(&delta);
                Content(delta)
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = parse_str_field(data, "item_id");
            if item_id.is_empty() {
                return Ignored;
            }
            let delta = parse_str_field(data, "delta");
            let slot = match accum.calls.iter_mut().find(|c| c.item_id == item_id) {
                Some(slot) => slot,
                None => {
                    accum.calls.push(ResponsesPendingCall {
                        item_id: item_id.clone(),
                        ..ResponsesPendingCall::default()
                    });
                    accum.calls.last_mut().expect("just pushed")
                }
            };
            slot.arguments.push_str(&delta);
            Ignored
        }
        "response.output_item.added" => {
            let item = serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|v| v.get("item").cloned())
                .unwrap_or(serde_json::Value::Null);
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                return Ignored;
            }
            let item_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            if item_id.is_empty() || accum.calls.iter().any(|c| c.item_id == item_id) {
                return Ignored;
            }
            accum.calls.push(ResponsesPendingCall {
                item_id,
                id: item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                name: item.get("name").and_then(|v| v.as_str()).map(str::to_owned),
                arguments: String::new(),
            });
            Ignored
        }
        "response.output_item.done" => {
            let item: Option<ResponsesOutputItem> = serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|v| v.get("item").cloned())
                .and_then(|item| serde_json::from_value(item).ok());
            let Some(item) = item else {
                return Ignored;
            };
            match item.kind.as_str() {
                "message" => {
                    let text: String = item
                        .content
                        .into_iter()
                        .filter(|part| part.kind == "output_text")
                        .filter_map(|part| part.text)
                        .collect();
                    if !text.is_empty() {
                        accum.done_texts.push(text);
                    }
                    Ignored
                }
                "function_call" => {
                    let key = item.id.clone().unwrap_or_default();
                    if let Some(slot) = accum.calls.iter_mut().find(|c| c.item_id == key) {
                        if slot.id.is_none() {
                            slot.id = item.call_id;
                        }
                        if slot.name.is_none() {
                            slot.name = item.name;
                        }
                        if slot.arguments.is_empty()
                            && let Some(args) = item.arguments
                        {
                            slot.arguments = args;
                        }
                    }
                    Ignored
                }
                _ => Ignored,
            }
        }
        "response.completed" | "response.incomplete" => Finished,
        "response.failed" => {
            let reason = serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|v| {
                    v.get("response")
                        .and_then(|r| r.get("error"))
                        .or_else(|| v.get("error"))
                        .and_then(responses_error_message)
                })
                .unwrap_or_else(|| "response failed".to_owned());
            Failed(reason)
        }
        _ => Ignored,
    }
}

/// Extracts a displayable reason from a Responses `error` payload, which
/// may be a plain string or an object carrying `message`.
pub fn responses_error_message(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::String(_) => None,
        _ => value
            .get("message")
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let s = value.to_string();
                if s.is_empty() || s == "null" {
                    None
                } else {
                    Some(s)
                }
            }),
    }
}

fn parse_str_field(data: &str, field: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get(field)?.as_str()?.to_owned().into())
        .unwrap_or_default()
}

/// Terminal state of [`fold_responses_bytes`].
#[derive(Debug, PartialEq, Eq)]
pub enum ResponsesFoldEnd {
    Ongoing,
    Finished,
    Failed(String),
}

/// Folds newly arrived bytes: decodes complete `\n`-terminated lines (kept
/// as raw bytes until the newline so split multibyte characters survive)
/// and feeds each `data:` line to the accumulator under the most recent
/// `event:` line. `pending` carries an unterminated event name across
/// chunk boundaries. Returns emitted text slices and the terminal state.
pub fn fold_responses_bytes(
    buffer: &mut Vec<u8>,
    bytes: &[u8],
    pending: &mut Option<String>,
    accum: &mut ResponsesAccum,
) -> (Vec<String>, ResponsesFoldEnd) {
    use ResponsesFoldEnd::*;
    buffer.extend_from_slice(bytes);
    let mut out = Vec::new();
    let mut end = Ongoing;
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=pos).collect();
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim();
        if line.is_empty() {
            *pending = None;
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            *pending = Some(name.trim().to_owned());
            continue;
        }
        if let Some(payload) = line.strip_prefix("data:") {
            let event = pending.clone().unwrap_or_default();
            match handle_responses_event(&event, payload, accum) {
                ResponsesLineOutcome::Content(text) => out.push(text),
                ResponsesLineOutcome::Finished => {
                    end = Finished;
                    break;
                }
                ResponsesLineOutcome::Failed(reason) => {
                    end = Failed(reason);
                    break;
                }
                ResponsesLineOutcome::Ignored => {}
            }
        }
    }
    (out, end)
}

/// Builds the final message from a finished Responses stream, or `None`
/// when it carried nothing (mirrors the `EmptyContent` error of
/// non-streaming completions). Prefers streamed deltas; falls back to
/// completed message texts when a stream carries no deltas.
pub fn assemble_responses_stream(accum: ResponsesAccum) -> Option<Message> {
    let content = if accum.content.is_empty() {
        accum.done_texts.concat()
    } else {
        accum.content
    };
    let calls: Vec<ToolCall> = accum
        .calls
        .into_iter()
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: call
                .id
                .filter(|s| !s.is_empty())
                .or(if call.item_id.is_empty() {
                    None
                } else {
                    Some(call.item_id)
                })
                .unwrap_or_else(|| format!("responses-call-{index}")),
            name: call.name.unwrap_or_default(),
            arguments: call.arguments,
        })
        .collect();
    if content.is_empty() && calls.is_empty() {
        return None;
    }
    Some(Message::assistant_with_calls(content, calls))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(models: &[&str]) -> Vec<String> {
        models.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn routing_defaults_to_chat_completions() {
        assert_eq!(
            transport_for("mimo-v2.5-free", &extra(&[])),
            Transport::ChatCompletions
        );
        assert_eq!(
            transport_for("gpt-5.4", &extra(&[])),
            Transport::ChatCompletions
        );
        assert_eq!(transport_for("", &extra(&[])), Transport::ChatCompletions);
    }

    #[test]
    fn routing_sends_muse_spark_family_to_responses() {
        for model in [
            "muse-spark-1.2",
            "muse-spark-1.2-contributor",
            "muse-spark-1.2-contributor-free",
            "muse-spark-1.3",
            "muse-spark-1.3-contributor-free",
        ] {
            assert_eq!(
                transport_for(model, &extra(&[])),
                Transport::Responses,
                "model {model}"
            );
        }
    }

    #[test]
    fn routing_config_extras_cover_future_models() {
        assert_eq!(
            transport_for("future-1", &extra(&["future-1"])),
            Transport::Responses
        );
        assert_eq!(
            transport_for("future-2", &extra(&["future-1"])),
            Transport::ChatCompletions
        );
    }

    #[test]
    fn request_body_marks_streaming() {
        let msg = Message::user("hi");
        let body = responses_body("m", std::slice::from_ref(&msg), &[], true);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "m");
        assert_eq!(json["stream"], true);
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn request_body_omits_stream_when_false() {
        let msg = Message::user("hi");
        let body = responses_body("m", std::slice::from_ref(&msg), &[], false);
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("stream").is_none(), "non-streaming body minimal");
    }

    #[test]
    fn input_converts_all_roles_and_tool_history() {
        let call = ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{\"text\":\"hi\"}".into(),
        };
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant_with_calls("thinking", vec![call]),
            Message::tool_result("c1", "hi"),
        ];
        let input = responses_input(&messages);
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["name"], "echo");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "c1");
        assert_eq!(input[4]["output"], "hi");
    }

    #[test]
    fn assemble_output_ignores_reasoning_and_maps_calls() {
        let items: Vec<ResponsesOutputItem> = serde_json::from_value(serde_json::json!([
            {"type": "reasoning", "id": "rs_1", "status": "completed"},
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "hel"},
                         {"type": "output_text", "text": "lo"}]},
            {"type": "function_call", "id": "x", "name": "echo",
             "call_id": "call_1", "arguments": "{\"text\":\"hi\"}"},
        ]))
        .unwrap();
        let msg = assemble_responses_output(items).expect("assembled");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id, "call_1");
        assert_eq!(msg.tool_calls[0].name, "echo");
    }

    #[test]
    fn assemble_output_needs_some_signal() {
        let empty: Vec<ResponsesOutputItem> = Vec::new();
        assert!(assemble_responses_output(empty).is_none());
        let reasoning: Vec<ResponsesOutputItem> = serde_json::from_value(serde_json::json!([
            {"type": "reasoning", "id": "rs_1", "status": "completed"},
        ]))
        .unwrap();
        assert!(assemble_responses_output(reasoning).is_none());
    }

    #[test]
    fn events_fold_text_and_function_calls() {
        let mut accum = ResponsesAccum::default();
        let out = handle_responses_event(
            "response.output_text.delta",
            r#"{"delta":"hel"}"#,
            &mut accum,
        );
        assert_eq!(out, ResponsesLineOutcome::Content("hel".to_owned()));
        assert_eq!(accum.content, "hel");
        assert_eq!(
            handle_responses_event("response.created", r#"{"response":{}}"#, &mut accum),
            ResponsesLineOutcome::Ignored
        );
        assert_eq!(
            handle_responses_event("", "", &mut accum),
            ResponsesLineOutcome::Ignored,
            "blank data"
        );
        assert_eq!(
            handle_responses_event("response.output_text.delta", "not json", &mut accum),
            ResponsesLineOutcome::Ignored,
            "malformed"
        );
        // Function call registered on added, args streamed by item id.
        assert_eq!(
            handle_responses_event(
                "response.output_item.added",
                r#"{"item":{"id":"fc_1","type":"function_call","name":"echo","call_id":"call_1"}}"#,
                &mut accum,
            ),
            ResponsesLineOutcome::Ignored
        );
        assert_eq!(
            handle_responses_event(
                "response.function_call_arguments.delta",
                r#"{"item_id":"fc_1","delta":"{\"text\":"}"#,
                &mut accum,
            ),
            ResponsesLineOutcome::Ignored
        );
        assert_eq!(
            handle_responses_event(
                "response.function_call_arguments.delta",
                r#"{"item_id":"fc_1","delta":"\"hi\"}"}"#,
                &mut accum,
            ),
            ResponsesLineOutcome::Ignored
        );
        assert_eq!(
            handle_responses_event("response.completed", "{}", &mut accum),
            ResponsesLineOutcome::Finished
        );
        let msg = assemble_responses_stream(accum).expect("assembled");
        assert_eq!(msg.content, "hel");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id, "call_1");
        assert_eq!(msg.tool_calls[0].arguments, "{\"text\":\"hi\"}");
    }

    #[test]
    fn done_items_fill_gaps_without_double_counting() {
        let mut accum = ResponsesAccum::default();
        // No deltas at all: completed message text is the fallback.
        assert_eq!(
            handle_responses_event(
                "response.output_item.done",
                r#"{"item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"hi"}]}}"#,
                &mut accum,
            ),
            ResponsesLineOutcome::Ignored
        );
        // Call done without prior added: slot stays absent, nothing invented.
        assert_eq!(
            handle_responses_event(
                "response.output_item.done",
                r#"{"item":{"id":"fc_9","type":"function_call","status":"completed","name":"echo","call_id":"call_9","arguments":"{}"}}"#,
                &mut accum,
            ),
            ResponsesLineOutcome::Ignored
        );
        let msg = assemble_responses_stream(accum).expect("assembled");
        assert_eq!(msg.content, "hi");
        assert!(msg.tool_calls.is_empty());
    }

    #[test]
    fn failed_event_carries_reason() {
        let mut accum = ResponsesAccum::default();
        let out = handle_responses_event(
            "response.failed",
            r#"{"response":{"status":"failed","error":{"message":"boom"}}}"#,
            &mut accum,
        );
        assert_eq!(out, ResponsesLineOutcome::Failed("boom".to_owned()));
    }

    #[test]
    fn typeless_data_falls_back_to_payload_type() {
        let mut accum = ResponsesAccum::default();
        let out = handle_responses_event(
            "",
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            &mut accum,
        );
        assert_eq!(out, ResponsesLineOutcome::Content("hi".to_owned()));
    }

    #[test]
    fn fold_bytes_pairs_events_and_survives_splits() {
        let mut accum = ResponsesAccum::default();
        let mut buffer = Vec::new();
        let mut pending = None;
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hé\"}\n",
            "\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"name\":\"echo\",\"call_id\":\"call_1\"}}\n",
            "\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\"}\n",
            "\n",
        );
        // Split mid-line and inside the multibyte `é`.
        let bytes = stream.as_bytes();
        let e_off = bytes.windows(2).position(|w| w == "é".as_bytes()).unwrap();
        let mid = e_off + 1 + 40;
        let mut slices = Vec::new();
        let mut end = ResponsesFoldEnd::Ongoing;
        for piece in [&bytes[..e_off + 1], &bytes[e_off + 1..mid], &bytes[mid..]] {
            let (mut out, done) =
                fold_responses_bytes(&mut buffer, piece, &mut pending, &mut accum);
            slices.append(&mut out);
            if done != ResponsesFoldEnd::Ongoing {
                end = done;
                break;
            }
        }
        assert_eq!(end, ResponsesFoldEnd::Finished);
        assert_eq!(slices, vec!["hé"]);
        let msg = assemble_responses_stream(accum).expect("assembled");
        assert_eq!(msg.content, "hé");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id, "call_1");
    }

    #[test]
    fn fold_bytes_reports_failed_terminal() {
        let mut accum = ResponsesAccum::default();
        let mut buffer = Vec::new();
        let mut pending = None;
        let (slices, end) = fold_responses_bytes(
            &mut buffer,
            b"event: response.failed\ndata: {\"response\":{\"error\":\"nope\"}}\n\n",
            &mut pending,
            &mut accum,
        );
        assert!(slices.is_empty());
        assert_eq!(end, ResponsesFoldEnd::Failed("nope".to_owned()));
    }
}
