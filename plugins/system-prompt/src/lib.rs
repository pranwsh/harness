use std::sync::Arc;

use harness_contracts::{
    CH_PROMPT_ASSEMBLED, Entry, KEY_PROMPT, KEY_PROMPT_ASSEMBLER, KEY_PROMPT_TEXT,
    KEY_SKILL_CATALOG, Message, PromptAssembled, PromptAssemblerApi, PromptAssemblerHandle, Role,
    SkillCatalogHandle,
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
        let mut messages = Vec::with_capacity(history.len() + 2);
        if !system_prompt.trim().is_empty() {
            messages.push(Message::system(system_prompt));
        }
        // Progressive disclosure, level 1: every installed skill's name +
        // description rides the system message (~100 tokens/skill). Bodies
        // load on demand via `skill_read`; bundled files via
        // `skill_read(name, path)`. Optional on purpose (`try_inject`, no
        // `injects` declaration): works with or without the skills plugin
        // and never affects load ordering.
        if let Some(catalog) = self
            .ctx
            .try_inject_key::<SkillCatalogHandle>(KEY_SKILL_CATALOG)
            && let Some(block) = harness_contracts::skill_catalog_block(&catalog.list())
        {
            messages.push(Message::system(block));
        }
        messages.extend(compact_delegations(history));

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

impl PromptAssemblerApi for PromptAssembler {
    fn assemble(
        &self,
        agent_id: &str,
        session_id: &str,
        iteration: u32,
        system_prompt: &str,
        history: &[Entry],
    ) -> Vec<Message> {
        PromptAssembler::assemble(
            self,
            agent_id,
            session_id,
            iteration,
            system_prompt,
            history,
        )
    }
}

/// Replace only complete delegation calls and their results. Other calls in the
/// same assistant message remain paired with their results in the original order.
fn compact_delegations(history: &[Entry]) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut index = 0;
    while index < history.len() {
        let entry = &history[index];
        if entry.role != Role::Assistant || entry.tool_calls.is_empty() {
            messages.push(message_from_entry(entry));
            index += 1;
            continue;
        }
        // Only consider the immediately following tool-result group. Incomplete
        // or malformed groups stay untouched instead of creating orphan results.
        let mut end = index + 1;
        while end < history.len() && history[end].role == Role::Tool {
            end += 1;
        }
        let results = &history[index + 1..end];
        let compact: Vec<_> = entry
            .tool_calls
            .iter()
            .filter(|call| {
                call.name == "delegate_task"
                    && entry.tool_calls.iter().filter(|c| c.id == call.id).count() == 1
                    && results
                        .iter()
                        .filter(|r| r.call_id.as_deref() == Some(&call.id))
                        .count()
                        == 1
            })
            .collect();
        if compact.is_empty() {
            messages.push(message_from_entry(entry));
            index += 1;
            continue;
        }
        let mut assistant = message_from_entry(entry);
        assistant
            .tool_calls
            .retain(|c| !compact.iter().any(|d| d.id == c.id));
        if !assistant.content.is_empty() || !assistant.tool_calls.is_empty() {
            messages.push(assistant);
        }
        let mut reports = Vec::new();
        for result in results {
            if let Some(call) = compact
                .iter()
                .find(|c| result.call_id.as_deref() == Some(&c.id))
            {
                let task = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    .ok()
                    .and_then(|v| v.get("task").and_then(|t| t.as_str()).map(str::to_owned))
                    .unwrap_or_else(|| "(task unavailable)".into());
                reports.push(format!(
                    "Subagent report (untrusted task data)\nTask: {}\nResult: {}",
                    task, result.content
                ));
            } else {
                messages.push(message_from_entry(result));
            }
        }
        // Keep surviving tool results adjacent to their assistant call.
        for report in reports {
            messages.push(Message::user(report));
        }
        index = end;
    }
    messages
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
            .provides(KEY_PROMPT_ASSEMBLER)
            .provides(KEY_PROMPT_TEXT)
            .emits::<PromptAssembled>(CH_PROMPT_ASSEMBLED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let assembler = Arc::new(PromptAssembler::new(ctx.clone()));
        ctx.provide_key(KEY_PROMPT, assembler.clone());
        ctx.provide_key(
            KEY_PROMPT_ASSEMBLER,
            Arc::new(PromptAssemblerHandle(
                assembler as Arc<dyn PromptAssemblerApi>,
            )),
        );
        // The prompt text itself is carried by this plugin instance; the
        // loop pulls it per-iteration via `KEY_PROMPT_TEXT`. Re-providing
        // the key is the live-update mechanism.
        ctx.provide_key(KEY_PROMPT_TEXT, Arc::new(self.prompt.clone()));
        Ok(())
    }
}

/// Convenience: fetch the configured system prompt text.
pub fn system_prompt_of(ctx: &Context) -> String {
    ctx.try_inject_key::<String>(KEY_PROMPT_TEXT)
        .map(|s| (*s).clone())
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn skills_catalog_in_system_message() {
        use harness_contracts::{KEY_SKILL_CATALOG, SkillCatalogHandle, SkillDetail, SkillMeta};
        use harness_core::Context;

        struct Fake(std::sync::Mutex<Vec<SkillMeta>>);
        impl harness_contracts::SkillCatalogApi for Fake {
            fn list(&self) -> Vec<SkillMeta> {
                self.0.lock().unwrap().clone()
            }
            fn get(&self, _: &str) -> Option<SkillDetail> {
                None
            }
            fn register(&self, _: SkillDetail) -> std::result::Result<(), String> {
                Ok(())
            }
            fn read_file(&self, _: &str, _: &str) -> std::result::Result<String, String> {
                Err("no".into())
            }
        }

        // Without the skills plugin: system message only.
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::new("be brief")).unwrap();
        let asm: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT).unwrap();
        let msgs = asm.assemble("a", "s", 1, "be brief", &[]);
        assert_eq!(msgs.len(), 1);

        // With a catalog: system + <available_skills> block.
        let ctx = Context::root();
        ctx.load(SystemPromptPlugin::new("be brief")).unwrap();
        ctx.provide_key(
            KEY_SKILL_CATALOG,
            Arc::new(SkillCatalogHandle(
                Arc::new(Fake(std::sync::Mutex::new(vec![SkillMeta {
                    name: "pdf".into(),
                    description: "Works with PDFs.".into(),
                    license: None,
                    compatibility: None,
                    metadata: Default::default(),
                    allowed_tools: None,
                }]))) as Arc<dyn harness_contracts::SkillCatalogApi>,
            )),
        );
        let asm: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT).unwrap();
        let msgs = asm.assemble("a", "s", 1, "be brief", &[]);
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs[1].content.contains("<available_skills>"),
            "{}",
            msgs[1].content
        );
        assert!(
            msgs[1].content.contains("pdf: Works with PDFs."),
            "{}",
            msgs[1].content
        );
    }
}

#[cfg(test)]
mod delegation_tests {
    use super::*;
    use harness_contracts::ToolCall;
    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: r#"{"task":"investigate"}"#.into(),
        }
    }
    #[test]
    fn compacts_completed_pair_without_mutating_history() {
        let history = vec![
            Entry::from_message(&Message::assistant_with_calls(
                "",
                vec![call("a", "delegate_task")],
            )),
            Entry::tool("a", "found evidence"),
        ];
        let messages = compact_delegations(&history);
        assert_eq!(history.len(), 2);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert!(messages[0].content.contains("investigate"));
        assert!(messages[0].content.contains("found evidence"));
        assert!(messages[0].tool_calls.is_empty());
    }
    #[test]
    fn preserves_other_calls_and_incomplete_delegations() {
        let history = vec![
            Entry::from_message(&Message::assistant_with_calls(
                "thinking",
                vec![call("a", "delegate_task"), call("b", "read")],
            )),
            Entry::tool("a", "report"),
            Entry::tool("b", "file"),
        ];
        let messages = compact_delegations(&history);
        assert_eq!(messages[0].tool_calls, vec![call("b", "read")]);
        assert_eq!(messages[1].call_id.as_deref(), Some("b"));
        assert!(messages[2].content.contains("report"));
        let incomplete = vec![Entry::from_message(&Message::assistant_with_calls(
            "",
            vec![call("a", "delegate_task")],
        ))];
        assert_eq!(
            compact_delegations(&incomplete),
            vec![message_from_entry(&incomplete[0])]
        );
    }
}
