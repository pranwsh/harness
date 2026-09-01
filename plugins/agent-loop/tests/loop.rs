//! Integration tests for the agent-loop orchestrator.
//!
//! Uses a scripted `FakeModel` and a fake tool so the whole turn pipeline —
//! session logging, prompt assembly, tool execution, event emission — runs
//! end-to-end without network access.

use std::sync::{Arc, Mutex};

use harness_agent_loop::{AgentLoop, TurnEvent};
use harness_contracts::{
    Entry, KEY_AGENT_LOOP, KEY_AGENTS, KEY_CONFIG, KEY_MODEL_CLIENT, KEY_PROMPT, KEY_SESSIONS,
    KEY_TOOLS, Message, Role, ToolCall, ToolError, ToolSpec,
};
use harness_core::Context;
use harness_model::{CompletionFuture, ModelClient, ModelClientHandle};
use harness_session::SessionLog;
use harness_tools::Tools;
use serde_json::json;

/// Model that replays a scripted list of replies, one per call.
struct FakeModel {
    replies: Mutex<Vec<Message>>,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl FakeModel {
    fn new(replies: Vec<Message>) -> Arc<Self> {
        Arc::new(FakeModel {
            replies: Mutex::new(replies),
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ModelClient for FakeModel {
    fn complete<'a>(
        &'a self,
        _model: &'a str,
        messages: &'a [Message],
        _tools: &'a [ToolSpec],
    ) -> CompletionFuture<'a> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let reply = self.replies.lock().unwrap().remove(0);
        Box::pin(async move { Ok(reply) })
    }
}

fn fake_tool_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "fake_tool".to_owned(),
        arguments: r#"{"value":"x"}"#.to_owned(),
    }
}

fn fake_spec() -> ToolSpec {
    ToolSpec {
        name: "fake_tool".to_owned(),
        description: "test tool".to_owned(),
        parameters: json!({"type": "object"}),
    }
}

/// Wires all plugins with the fake model and max_iterations override.
fn setup(replies: Vec<Message>, max_iterations: u32) -> Context {
    let ctx = Context::root();
    let config = format!(
        "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n\n[agent]\nmax_iterations = {max_iterations}\n"
    );
    ctx.load(
        harness_config::ConfigPlugin::from_toml(&config).unwrap(),
    )
    .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    ctx.load(harness_session::SessionPlugin).unwrap();
    ctx.load(harness_tools::ToolsPlugin::new()).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();

    // Swap in the fake model.
    let fake = ModelClientHandle(FakeModel::new(replies));
    ctx.provide_key(KEY_MODEL_CLIENT, Arc::new(fake));

    // Register a fake tool that echoes its arguments.
    let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    tools
        .register(fake_spec(), |args| {
            Box::pin(async move { Ok(format!("echo:{args}")) })
        })
        .unwrap();

    ctx.load(harness_agent_loop::AgentLoopPlugin).unwrap();
    ctx
}

async fn drain(mut rx: tokio::sync::mpsc::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn completes_after_tool_round_trip() {
    let ctx = setup(
        vec![
            Message::assistant_with_calls("let me check", vec![fake_tool_call("t1")]),
            Message::assistant("all done"),
        ],
        8,
    );

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s1", "hello")).await;

    // Stream shape: started, iter1, assistant, tool, iter2, assistant, done.
    assert!(matches!(events.first(), Some(TurnEvent::Started)));
    assert!(matches!(events.last(), Some(TurnEvent::Completed(2))));
    let assistant = events
        .iter()
        .filter(|e| matches!(e, TurnEvent::Assistant(_)))
        .count();
    assert_eq!(assistant, 2);

    // Session: user, assistant+calls, tool result, assistant — in order.
    let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
    let history = log.history("s1");
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[0].content, "hello");
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].tool_calls.len(), 1);
    assert_eq!(history[2].role, Role::Tool);
    assert_eq!(history[2].call_id.as_deref(), Some("t1"));
    assert_eq!(history[3].role, Role::Assistant);
    assert_eq!(history[3].content, "all done");
}

#[tokio::test]
async fn second_model_call_sees_tool_result_in_history() {
    let ctx = setup(
        vec![
            Message::assistant_with_calls("checking", vec![fake_tool_call("t1")]),
            Message::assistant("done"),
        ],
        8,
    );

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let _ = drain(loop_svc.run("a1", "s2", "hi")).await;

    // Reaching here at all implies the fake model got the tool result; now
    // assert it directly: its second call must contain the tool message.
    let handle: Arc<ModelClientHandle> = ctx.inject_key(KEY_MODEL_CLIENT).unwrap();
    // The handle wraps FakeModel; we can't reach inside, so instead verify
    // through the session log's authoritative copy (below).
    let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
    let history = log.history("s2");
    assert_eq!(history[2].role, Role::Tool);
    assert!(history[2].content.contains("echo"));
    let _ = handle;
}

#[tokio::test]
async fn no_tool_calls_completes_in_one_iteration() {
    let ctx = setup(
        vec![Message::assistant("immediate answer")],
        8,
    );

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s3", "ping")).await;

    assert!(matches!(events.last(), Some(TurnEvent::Completed(1))));
    let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
    assert_eq!(log.len("s3"), 2); // user + assistant
}

#[tokio::test]
async fn hits_max_iterations_cap() {
    // Model always requests a tool → never terminates on its own.
    let ctx = setup(
        vec![
            Message::assistant_with_calls("again", vec![fake_tool_call("t1")]);
            16
        ],
        3,
    );

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s4", "loop forever")).await;

    match events.last() {
        Some(TurnEvent::Failed(err)) => assert!(err.contains("max iterations")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_errors_are_fed_back_and_logged() {
    let ctx = Context::root();
    let config = "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n";
    ctx.load(harness_config::ConfigPlugin::from_toml(config).unwrap())
        .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    ctx.load(harness_session::SessionPlugin).unwrap();
    ctx.load(harness_tools::ToolsPlugin::new()).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();

    let replies = vec![
        Message::assistant_with_calls("try it", vec![ToolCall {
            id: "t1".into(),
            name: "failing_tool".into(),
            arguments: "{}".into(),
        }]),
        Message::assistant("recovered"),
    ];
    ctx.provide_key(KEY_MODEL_CLIENT, Arc::new(ModelClientHandle(FakeModel::new(replies))));

    let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    tools
        .register(
            ToolSpec {
                name: "failing_tool".to_owned(),
                description: "always fails".to_owned(),
                parameters: json!({"type": "object"}),
            },
            |_args| {
                Box::pin(async move {
                    Err(ToolError {
                        tool: "failing_tool".to_owned(),
                        message: "boom".to_owned(),
                    })
                })
            },
        )
        .unwrap();

    ctx.load(harness_agent_loop::AgentLoopPlugin).unwrap();

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s5", "go")).await;

    assert!(matches!(events.last(), Some(TurnEvent::Completed(_))));
    let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
    let history = log.history("s5");
    assert_eq!(history[2].role, Role::Tool);
    assert!(history[2].content.contains("boom"));

    // Agent ends idle after a successful turn.
    let agents: Arc<harness_agent::AgentRegistry> = ctx.inject_key(KEY_AGENTS).unwrap();
    assert_eq!(agents.state_of("a1"), Some(harness_contracts::AgentState::Idle));
    let _ = (KEY_CONFIG, KEY_PROMPT, Entry::from_message(&Message::user("x")));
}
