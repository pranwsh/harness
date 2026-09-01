use std::sync::Arc;

use harness_contracts::{
    CH_PROMPT_ASSEMBLED, Entry, KEY_PROMPT, Message, PromptAssembled, Role,
};
use harness_core::{Context, Result};

/// Builds the message array sent to the model from session history.
///
/// Assembly is a pure function of history: system prompt first, then the
/// conversation. State markers are absent by construction (the session log
/// only stores conversation entries in v1).
pub struct PromptAssembler {
    ctx: Context,
}

impl PromptAssembler {
    pub fn new(ctx: Context) -> Self {
        PromptAssembler { ctx }
    }

    /// Assembles the messages for one model call and emits `prompt.assembled`
    /// for debugging/telemetry.
    pub fn assemble(
        &self,
        agent_id: &str,
        session_id: &str,
        iteration: u32,
        system_prompt: &str,
        history: &[Entry],
    ) -> Vec<Message> {
        let mut messages = Vec::with_capacity(history.len() + 1);
        if !system_prompt.trim().is_empty() {
            messages.push(Message::system(system_prompt));
        }
        for entry in history {
            messages.push(message_from_entry(entry));
        }

        let _ = self.ctx.emit_key(
            CH_PROMPT_ASSEMBLED,
            PromptAssembled {
                agent_id: agent_id.to_owned(),
                iteration,
                prompt: render(&messages, session_id),
            },
        );
        messages
    }
}

fn message_from_entry(entry: &Entry) -> Message {
    Message {
        role: entry.role,
        content: entry.content.clone(),
        tool_calls: entry.tool_calls.clone(),
        call_id: entry.call_id.clone(),
    }
}

fn render(messages: &[Message], session_id: &str) -> String {
    let mut out = format!("session {session_id}:\n");
    for m in messages {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        out.push_str(&format!("[{role}] {}\n", m.content));
    }
    out
}

pub struct SystemPromptPlugin {
    prompt: String,
}

impl SystemPromptPlugin {
    pub fn new(prompt: impl Into<String>) -> Self {
        SystemPromptPlugin {
            prompt: prompt.into(),
        }
    }
}

impl Default for SystemPromptPlugin {
    fn default() -> Self {
        Self::new(DEFAULT_SYSTEM_PROMPT)
    }
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful coding assistant running inside a plugin harness. Use tools when they help; answer concisely.";

impl harness_core::Plugin for SystemPromptPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("system-prompt")
            .provides(KEY_PROMPT)
            .emits::<PromptAssembled>(CH_PROMPT_ASSEMBLED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(
            KEY_PROMPT,
            Arc::new(PromptAssembler::new(ctx.clone())),
        );
        // The prompt text itself is carried by this plugin instance; the
        // loop queries it via `system_prompt()`.
        ctx.provide_key(
            "prompt.text",
            Arc::new(self.prompt.clone()),
        );
        Ok(())
    }
}

/// Convenience: fetch the configured system prompt text.
pub fn system_prompt_of(ctx: &Context) -> String {
    ctx.try_inject_key::<String>("prompt.text")
        .map(|s| (*s).clone())
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Plugin;

    #[test]
    fn assembles_system_then_history_in_order() {
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::new("be brief")).unwrap();
        let asm: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT).unwrap();

        let history = vec![
            Entry::from_message(&Message::user("hello")),
            Entry::from_message(&Message::assistant("hi there")),
        ];
        let msgs = asm.assemble("a", "s", 1, "be brief", &history);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "be brief");
        assert_eq!(msgs[1].content, "hello");
        assert_eq!(msgs[2].content, "hi there");
    }

    #[test]
    fn empty_system_prompt_is_omitted() {
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::default()).unwrap();
        let asm: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT).unwrap();

        let msgs = asm.assemble("a", "s", 1, "", &[]);
        assert!(msgs.is_empty());
    }

    #[test]
    fn tool_entries_carry_call_id() {
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::default()).unwrap();
        let asm: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT).unwrap();

        let history = vec![Entry::tool("t1", "result body")];
        let msgs = asm.assemble("a", "s", 1, "sys", &history);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, Role::Tool);
        assert_eq!(msgs[1].call_id.as_deref(), Some("t1"));
    }

    #[test]
    fn default_prompt_is_reachable() {
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::default()).unwrap();
        assert_eq!(system_prompt_of(&ctx), DEFAULT_SYSTEM_PROMPT);
    }
}
