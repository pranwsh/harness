//! Integration tests for the agent-loop orchestrator.
//!
//! Uses a scripted `FakeModel` and a fake tool so the whole turn pipeline —
//! session logging, prompt assembly, tool execution, event emission — runs
//! end-to-end without network access.

use std::sync::{Arc, Mutex};

use harness_agent_loop::{AgentLoop, TurnEvent};
use harness_contracts::{
    AgentRegistryHandle, Entry, KEY_AGENT_LOOP, KEY_AGENTS_API, KEY_CONFIG, KEY_MODEL_STREAMER,
    KEY_PROMPT, KEY_SESSION_STORE, KEY_TOOLS, Message, ModelStreamerApi, ModelStreamerHandle,
    Role, SessionStoreHandle, StreamEvent, ToolCall, ToolError, ToolSpec,
};
use harness_core::Context;
use harness_tools::Tools;
use serde_json::json;

/// Model that replays a scripted list of replies, one per call.
///
/// Implements the decoupled [`ModelStreamerApi`] (not the model crate's
/// `ModelClient`), mirroring its default chunked replay: each reply
/// surfaces as one `Content` slice plus `Done`.
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

impl ModelStreamerApi for FakeModel {
    fn stream(
        self: Arc<Self>,
        _model: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> tokio::sync::mpsc::Receiver<StreamEvent> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let reply = self.replies.lock().unwrap().remove(0);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            if !reply.content.is_empty() {
                let _ = tx.send(StreamEvent::Content(reply.content.clone())).await;
            }
            let _ = tx.send(StreamEvent::Done(reply)).await;
        });
        rx
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
    ctx.load(harness_config::ConfigPlugin::from_toml(&config).unwrap())
        .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    ctx.load(harness_session::SessionPlugin).unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();

    // Swap in the fake model via the decoupled streamer handle.
    let fake = FakeModel::new(replies);
    ctx.provide_key(
        KEY_MODEL_STREAMER,
        Arc::new(ModelStreamerHandle(fake as Arc<dyn ModelStreamerApi>)),
    );

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

    // Stream shape: started, iter1, deltas, tool, iter2, deltas, done.
    // The fake model replays each reply as one slice via the default
    // `stream` impl, so each reply surfaces as exactly one delta.
    assert!(matches!(events.first(), Some(TurnEvent::Started)));
    assert!(matches!(events.last(), Some(TurnEvent::Completed(2))));
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::AssistantDelta(d) => Some(d.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["let me check", "all done"]);

    // Session: user, assistant+calls, tool result, assistant — in order.
    let log: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE).unwrap();
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
    // assert it directly through the session store's authoritative copy.
    let log: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE).unwrap();
    let history = log.history("s2");
    assert_eq!(history[2].role, Role::Tool);
    assert!(history[2].content.contains("echo"));
}

#[tokio::test]
async fn no_tool_calls_completes_in_one_iteration() {
    let ctx = setup(vec![Message::assistant("immediate answer")], 8);

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s3", "ping")).await;

    assert!(matches!(events.last(), Some(TurnEvent::Completed(1))));
    let log: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE).unwrap();
    assert_eq!(log.len("s3"), 2); // user + assistant
}

#[tokio::test]
async fn hits_max_iterations_cap() {
    // Model always requests a tool → never terminates on its own.
    let ctx = setup(
        vec![Message::assistant_with_calls("again", vec![fake_tool_call("t1")]); 16],
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
async fn prompt_text_is_pulled_fresh_each_turn() {
    use harness_contracts::KEY_PROMPT_TEXT;

    // Fake model records every message list it receives.
    let fake = FakeModel::new(vec![
        Message::assistant("first done"),
        Message::assistant("second done"),
    ]);
    let seen = Arc::clone(&fake);

    let ctx = Context::root();
    let config = "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n";
    ctx.load(harness_config::ConfigPlugin::from_toml(config).unwrap())
        .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    ctx.load(harness_session::SessionPlugin).unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::new("prompt-v1"))
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();
    ctx.provide_key(
        KEY_MODEL_STREAMER,
        Arc::new(ModelStreamerHandle(fake as Arc<dyn ModelStreamerApi>)),
    );
    ctx.load(harness_agent_loop::AgentLoopPlugin).unwrap();

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let _ = drain(loop_svc.run("a1", "s-prompt", "hi")).await;

    // Live-update the prompt text by re-providing the service key.
    ctx.provide_key(
        KEY_PROMPT_TEXT,
        Arc::new("prompt-v2-live".to_owned()),
    );

    let _ = drain(loop_svc.run("a1", "s-prompt", "again")).await;

    let seen = seen.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "expected one model call per turn");
    // First call carries v1, second carries the live-updated v2.
    assert_eq!(seen[0][0].role, Role::System);
    assert_eq!(seen[0][0].content, "prompt-v1");
    assert_eq!(seen[1][0].role, Role::System);
    assert_eq!(seen[1][0].content, "prompt-v2-live");
}

#[tokio::test]
async fn max_iterations_is_pulled_fresh_each_turn() {
    // Looping model: always requests a tool so the cap decides the outcome.
    let fake = FakeModel::new(vec![
        Message::assistant_with_calls("again", vec![fake_tool_call("t1")]);
        16
    ]);

    let ctx = Context::root();
    let config = "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n\n[agent]\nmax_iterations = 8\n";
    ctx.load(harness_config::ConfigPlugin::from_toml(config).unwrap())
        .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    ctx.load(harness_session::SessionPlugin).unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();
    ctx.provide_key(
        KEY_MODEL_STREAMER,
        Arc::new(ModelStreamerHandle(fake as Arc<dyn ModelStreamerApi>)),
    );
    let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    tools
        .register(fake_spec(), |args| {
            Box::pin(async move { Ok(format!("echo:{args}")) })
        })
        .unwrap();
    ctx.load(harness_agent_loop::AgentLoopPlugin).unwrap();

    // Tighten the bound live: next turn must fail after 1 iteration.
    // Single source of truth — one key holds the live config handle.
    use harness_contracts::{ConfigApi, ConfigHandle, KEY_CONFIG};
    let tightened = harness_contracts::AppConfig::from_toml(
        "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n\n[agent]\nmax_iterations = 1\n",
        "test",
    )
    .unwrap();
    ctx.provide_key(
        KEY_CONFIG,
        Arc::new(ConfigHandle(Arc::new(tightened) as Arc<dyn ConfigApi>)),
    );

    let loop_svc: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP).unwrap();
    let events = drain(loop_svc.run("a1", "s-cap", "loop forever")).await;
    match events.last() {
        Some(TurnEvent::Failed(err)) => assert!(
            err.contains("max iterations (1)"),
            "expected live cap of 1, got: {err}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_session_listener_fails_closed() {
    // No session plugin loaded: user append must fail loudly instead of
    // silently dropping the entry.
    let ctx = Context::root();
    let config = "[llm]\nbase_url=\"u\"\nmodel=\"fake\"\napi_key=\"k\"\nuser_agent=\"a\"\n";
    ctx.load(harness_config::ConfigPlugin::from_toml(config).unwrap())
        .unwrap();
    ctx.load(harness_model::ModelPlugin).unwrap();
    ctx.load(harness_agent::AgentPlugin).unwrap();
    // NOTE: session plugin intentionally not loaded.
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();
    ctx.provide_key(
        KEY_MODEL_STREAMER,
        Arc::new(ModelStreamerHandle(
            FakeModel::new(vec![Message::assistant("unreachable")])
                as Arc<dyn ModelStreamerApi>,
        )),
    );
    // Agent-loop parks while sessions.store is missing; the pending outcome
    // proves the fail-closed path instead of silently dropping entries.
    let outcome = ctx.load(harness_agent_loop::AgentLoopPlugin).unwrap();
    assert!(
        matches!(outcome, harness_core::LoadOutcome::Pending { .. }),
        "loop must park without session, got: {outcome:?}"
    );
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
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_system_prompt::SystemPromptPlugin::default())
        .unwrap();
    ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
        .unwrap();

    let replies = vec![
        Message::assistant_with_calls(
            "try it",
            vec![ToolCall {
                id: "t1".into(),
                name: "failing_tool".into(),
                arguments: "{}".into(),
            }],
        ),
        Message::assistant("recovered"),
    ];
    ctx.provide_key(
        KEY_MODEL_STREAMER,
        Arc::new(ModelStreamerHandle(
            FakeModel::new(replies) as Arc<dyn ModelStreamerApi>
        )),
    );

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
    let log: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE).unwrap();
    let history = log.history("s5");
    assert_eq!(history[2].role, Role::Tool);
    assert!(history[2].content.contains("boom"));

    // Agent ends idle after a successful turn.
    let agents: Arc<AgentRegistryHandle> = ctx.inject_key(KEY_AGENTS_API).unwrap();
    assert_eq!(
        agents.state_of("a1"),
        Some(harness_contracts::AgentState::Idle)
    );
    let _ = (
        KEY_CONFIG,
        KEY_PROMPT,
        Entry::from_message(&Message::user("x")),
    );
}
