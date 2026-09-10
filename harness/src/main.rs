use std::{process::ExitCode, sync::Arc};

use harness_config::{ConfigPlugin, config_path};
use harness_contracts::KEY_TUI;
use harness_core::{Context, LoadOutcome, Result};
use tokio::sync::mpsc;

mod cli;

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
    let cli = match cli::parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{err}");
            return Ok(ExitCode::FAILURE);
        }
    };

    let ctx = Context::root();

    let path = cli.config.unwrap_or_else(config_path);
    let config = match ConfigPlugin::from_file(&path) {
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
    // Optional: injects/validates LLM headers via the request waterfall and
    // observes response headers. Omit with no effect on requests.
    load!(
        ctx,
        "model-headers",
        harness_model_headers::ModelHeadersPlugin
    );
    load!(ctx, "agent", harness_agent::AgentPlugin);
    load!(ctx, "session", harness_session::SessionPlugin);
    load!(ctx, "tools", harness_tools::ToolsPlugin);
    load!(ctx, "hash-base", harness_hash_base::HashBasePlugin);
    load!(
        ctx,
        "hashline-read",
        harness_hashline_read::HashlineReadPlugin
    );
    load!(
        ctx,
        "hashline-edit",
        harness_hashline_edit::HashlineEditPlugin
    );
    {
        // Shell injects `config.app` + `tools.executor` via DI and parks
        // until both are ready; no ctor wiring needed.
        load!(ctx, "shell", harness_shell::ShellPlugin);
    }
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
    load!(ctx, "tui-input", harness_tui_input::InputPlugin);
    load!(ctx, "tui-markdown", harness_tui_markdown::MarkdownPlugin);
    load!(ctx, "tui-popup", harness_tui_popup::TuiPopupPlugin);
    load!(ctx, "tui-model", harness_tui_model::TuiModelPlugin);
    load!(ctx, "tui", harness_tui::TuiPlugin::new(done_tx));

    let tui: Arc<harness_tui::Tui> = ctx.inject_key(KEY_TUI)?;

    tui.run().await;
    let _ = done_rx.recv().await;

    Ok(ExitCode::SUCCESS)
}
