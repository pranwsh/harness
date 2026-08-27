use std::sync::Arc;

use harness_core::{Context, Plugin, PluginMeta};
use harness_llm::{ChatService, KEY_CHAT_SERVICE};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

pub const KEY_REPL_DONE: &str = "tui.done";

pub type ReplDone = Mutex<watch::Receiver<()>>;

pub struct ReplPlugin;

impl Plugin for ReplPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("repl")
            .injects(KEY_CHAT_SERVICE)
            .provides(KEY_REPL_DONE)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let chat: Arc<ChatService> = ctx.inject_key(KEY_CHAT_SERVICE)?;
        let (done_tx, done_rx) = watch::channel(());
        ctx.provide_key(KEY_REPL_DONE, Arc::new(Mutex::new(done_rx)));
        tokio::spawn(repl(chat, done_tx));
        Ok(())
    }
}

async fn repl(chat: Arc<ChatService>, done: watch::Sender<()>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        write_prompt().await;
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "/quit" {
                    break;
                }
                match chat.chat(line).await {
                    Ok(reply) => println!("{reply}"),
                    Err(err) => eprintln!("error: {err}"),
                }
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("error reading stdin: {err}");
                break;
            }
        }
    }
    let _ = done.send(());
}

async fn write_prompt() {
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(b"> ").await;
    let _ = stdout.flush().await;
}
