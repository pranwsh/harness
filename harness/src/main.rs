use std::{process::ExitCode, sync::Arc};

use harness_config::{ConfigPlugin, config_path};
use harness_core::{Context, LoadOutcome, Result};
use tokio::sync::mpsc;

macro_rules! load {
    ($ctx:expr, $name:literal, $plugin:expr) => {
        match $ctx.load($plugin) {
            Ok(LoadOutcome::Activated) => println!("{}: activated", $name),
            Ok(LoadOutcome::Pending { missing }) => {
                println!("{}: parked, missing {missing:?}", $name)
            }
            Err(err) => {
                eprintln!("harness: {} failed to load: {err}", $name);
                return Ok(ExitCode::FAILURE);
            }
        }
    };
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("harness: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    let ctx = Context::root();

    let config = match ConfigPlugin::from_file(&config_path()) {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("harness: {err}");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Load order is a convenience, not a requirement: the core parks any
    // plugin whose dependencies are not yet registered and cascades them in.
    load!(ctx, "config", config);
    load!(ctx, "model", harness_model::ModelPlugin);
    load!(ctx, "agent", harness_agent::AgentPlugin);
    load!(ctx, "session", harness_session::SessionPlugin);
    load!(ctx, "tools", harness_tools::ToolsPlugin::new());
    load!(
        ctx,
        "system-prompt",
        harness_system_prompt::SystemPromptPlugin::default()
    );
    load!(
        ctx,
        "agent-default-model",
        harness_agent_default_model::AgentDefaultModelPlugin
    );
    load!(ctx, "agent-loop", harness_agent_loop::AgentLoopPlugin);

    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    load!(ctx, "repl", harness_repl::ReplPlugin::new(done_tx));

    let repl: Arc<harness_repl::Repl> = ctx.inject_key("ui.repl")?;

    println!("ready — type a message, /history or /quit");
    repl.run().await;
    let _ = done_rx.recv().await;

    Ok(ExitCode::SUCCESS)
}
