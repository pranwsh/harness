use std::process::ExitCode;
use std::sync::Arc;

use harness_config::{ConfigPlugin, config_path};
use harness_core::{Context, LoadOutcome};
use harness_llm::LlmPlugin;
use harness_loop::CoreLoopPlugin;
use harness_tui::{KEY_REPL_DONE, ReplDone, ReplPlugin};

#[tokio::main]
async fn main() -> ExitCode {
    let ctx = Context::root();

    let config = match ConfigPlugin::from_file(&config_path()) {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("harness: {err}");
            return ExitCode::FAILURE;
        }
    };
    let llm = match LlmPlugin::new() {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("harness: failed to build http client: {err}");
            return ExitCode::FAILURE;
        }
    };

    let loads = [
        ("config-parser", ctx.load(config)),
        ("llm", ctx.load(llm)),
        ("core-loop", ctx.load(CoreLoopPlugin)),
        ("repl", ctx.load(ReplPlugin)),
    ];
    for (name, outcome) in loads {
        match outcome {
            Ok(LoadOutcome::Activated) => println!("{name}: activated"),
            Ok(LoadOutcome::Pending { missing }) => {
                println!("{name}: parked, missing {missing:?}")
            }
            Err(err) => {
                eprintln!("harness: {name} failed to load: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let done: Arc<ReplDone> = match ctx.inject_key(KEY_REPL_DONE) {
        Ok(done) => done,
        Err(err) => {
            eprintln!("harness: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("ready — type a message, /quit to exit");
    let repl_ended = async {
        let _ = done.lock().await.changed().await;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("harness: interrupted"),
        _ = repl_ended => println!("harness: repl ended"),
    }
    ExitCode::SUCCESS
}
