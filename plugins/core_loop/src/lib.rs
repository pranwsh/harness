use std::sync::Arc;

use harness_core::{Context, Plugin, PluginMeta};
use harness_llm::{ChatService, KEY_CHAT_SERVICE};

pub struct CoreLoopPlugin;

impl Plugin for CoreLoopPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("core-loop").injects(KEY_CHAT_SERVICE)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let _chat: Arc<ChatService> = ctx.inject_key(KEY_CHAT_SERVICE)?;
        Ok(())
    }
}
