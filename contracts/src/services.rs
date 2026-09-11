//! Service traits (DI contracts) for cross-plugin consumption.
//!
//! Concrete service structs live in their provider crates; consumers depend
//! only on these traits plus the sized [`Handle`] wrappers below, so the
//! crate graph stays acyclic (`harness-contracts` never depends on a
//! plugin). Providers implement the trait for their concrete type and
//! publish `Arc<Handle>` under the corresponding `KEY_*_API` key alongside
//! any legacy concrete service.
//!
//! Exceptions: the TUI leaf shell (`tui`/`tui-model`/`tui-input`/
//! `tui-markdown`/`tui-popup`/`tui-state`) and the shared hashing infra
//! (`hashline-read`/`hashline-edit` → `hash-base`) are the only allowed
//! direct plugin→plugin edges. Both are downward into leaves/bases, never
//! domain→domain, and keep the graph acyclic. See `contracts/src/lib.rs`
//! for rationale.

use std::sync::Arc;

use crate::{AgentState, Entry, Message, ToolCall, ToolError, ToolSpec};

/// Boxed future for object-safe async service methods.
pub type BoxFuture<T> = futures::future::BoxFuture<'static, T>;

// ---- model streaming ---------------------------------------------------------

/// One item of a streaming completion.
///
/// Moved here from the model plugin so loops can consume streams without
/// depending on the model crate. The model crate re-exports this type.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A slice of assistant text, to display immediately.
    Content(String),
    /// The stream ended; the fully assembled message (content + tool calls).
    Done(Message),
    /// The stream failed after `Content` may already have been emitted.
    Failed(String),
}

/// Minimal streaming surface the agent loop needs.
///
/// Object-safe via the `Arc<Self>` receiver. Providers with richer clients
/// (non-streaming `complete`, transport selection) keep those on their
/// concrete types; the loop only ever calls `stream`.
pub trait ModelStreamerApi: Send + Sync + 'static {
    fn stream(
        self: Arc<Self>,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> tokio::sync::mpsc::Receiver<StreamEvent>;
}

/// Sized handle so the service registry (`inject_key` requires `Sized`) can
/// carry `Arc<dyn ModelStreamerApi>`. Same idiom as the legacy
/// `ModelClientHandle`.
#[derive(Clone)]
pub struct ModelStreamerHandle(pub Arc<dyn ModelStreamerApi>);

impl std::ops::Deref for ModelStreamerHandle {
    type Target = dyn ModelStreamerApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- agent registry ----------------------------------------------------------

/// Lifecycle operations the loop needs. Deliberately narrower than the full
/// registry (no id generation); returns `()` so no provider-side `Agent`
/// type leaks into the contract.
pub trait AgentRegistryApi: Send + Sync + 'static {
    fn get_or_create(&self, id: &str);
    fn set_state(&self, id: &str, state: AgentState);
    fn state_of(&self, id: &str) -> Option<AgentState>;
}

#[derive(Clone)]
pub struct AgentRegistryHandle(pub Arc<dyn AgentRegistryApi>);

impl std::ops::Deref for AgentRegistryHandle {
    type Target = dyn AgentRegistryApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- session store -----------------------------------------------------------

/// Read + turn-bookkeeping surface the loop needs. Writes go through the
/// bus (`session.append_requested` / `tool.executed`) to the session
/// plugin, never through this trait — the session plugin is the sole
/// persistence owner.
pub trait SessionStoreApi: Send + Sync + 'static {
    fn begin_turn(&self, session_id: &str) -> u64;
    fn end_turn(&self, session_id: &str, turn: u64);
    fn history(&self, session_id: &str) -> Vec<Entry>;
    fn len(&self, session_id: &str) -> usize {
        self.history(session_id).len()
    }
}

#[derive(Clone)]
pub struct SessionStoreHandle(pub Arc<dyn SessionStoreApi>);

impl std::ops::Deref for SessionStoreHandle {
    type Target = dyn SessionStoreApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- tool executor -----------------------------------------------------------

/// Subset of the tools service the loop needs: advertise specs and execute
/// calls. Registration is via [`ToolRegistryApi`] for tool-provider plugins.
pub trait ToolExecutorApi: Send + Sync + 'static {
    fn specs(&self) -> Vec<ToolSpec>;
    fn execute(
        &self,
        agent_id: &str,
        session_id: &str,
        turn: u64,
        call: ToolCall,
    ) -> BoxFuture<Result<String, ToolError>>;
}

#[derive(Clone)]
pub struct ToolExecutorHandle(pub Arc<dyn ToolExecutorApi>);

impl std::ops::Deref for ToolExecutorHandle {
    type Target = dyn ToolExecutorApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Registry surface tool providers (shell, hashline-*) use to advertise
/// handlers. Providers depend only on this trait, never on the concrete
/// `Tools` type.
pub trait ToolRegistryApi: Send + Sync + 'static {
    fn register(
        &self,
        spec: ToolSpec,
        handler: Box<dyn Fn(String) -> BoxFuture<Result<String, ToolError>> + Send + Sync>,
    ) -> Result<(), String>;
}

#[derive(Clone)]
pub struct ToolRegistryHandle(pub Arc<dyn ToolRegistryApi>);

impl std::ops::Deref for ToolRegistryHandle {
    type Target = dyn ToolRegistryApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- prompt assembler --------------------------------------------------------

/// Pure assembly of the model message list from history. The system-prompt
/// text itself travels separately as `Arc<String>` under `KEY_PROMPT_TEXT`
/// and is pulled per-iteration by the loop.
pub trait PromptAssemblerApi: Send + Sync + 'static {
    fn assemble(
        &self,
        agent_id: &str,
        session_id: &str,
        iteration: u32,
        system_prompt: &str,
        history: &[Entry],
    ) -> Vec<Message>;
}

#[derive(Clone)]
pub struct PromptAssemblerHandle(pub Arc<dyn PromptAssemblerApi>);

impl std::ops::Deref for PromptAssemblerHandle {
    type Target = dyn PromptAssemblerApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- model selector ----------------------------------------------------------

/// Model choice for one iteration. Catalog management, persistence, and
/// refresh stay on the concrete `ModelSelector` for the `/model` popup
/// bridge (TUI shell) and are intentionally absent here.
pub trait ModelSelectorApi: Send + Sync + 'static {
    fn select(&self, agent_id: &str) -> String;
}

#[derive(Clone)]
pub struct ModelSelectorHandle(pub Arc<dyn ModelSelectorApi>);

/// Rich catalog surface for the `/model` popup (TUI leaf). The agent loop
/// only needs `select`; the popup needs catalog mutation and refresh.
pub trait ModelCatalogApi: Send + Sync + 'static {
    fn current(&self) -> String;
    fn default_model(&self) -> String;
    fn models(&self) -> Vec<String>;
    fn set_current(&self, model: &str);
    fn set_catalog(&self, models: Vec<String>);
    fn snapshot(&self) -> (String, Vec<String>);
    fn restore(&self, current: String, catalog: Vec<String>);
    fn sync_default(&self, model: &str);
    fn refresh(self: Arc<Self>) -> BoxFuture<Option<Vec<String>>>;
}

#[derive(Clone)]
pub struct ModelCatalogHandle(pub Arc<dyn ModelCatalogApi>);

impl std::ops::Deref for ModelCatalogHandle {
    type Target = dyn ModelCatalogApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl std::ops::Deref for ModelSelectorHandle {
    type Target = dyn ModelSelectorApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// ---- config (single source of truth) ---------------------------------------

/// Unified config surface. The single key `config.app` holds an
/// `Arc<ConfigHandle>` over `ConfigService`; all readers derive from it
/// (`get()` for full snapshots, `max_iterations()` for the loop,
/// `set_llm_model()` for the `/model` popup). Keeps `AppConfig` snapshots
/// and live bounds from diverging.
pub trait ConfigApi: Send + Sync + 'static {
    fn get(&self) -> crate::config::AppConfig;
    fn max_iterations(&self) -> u32 {
        self.get().agent.max_iterations
    }
    fn set_llm_model(&self, model: &str) -> Result<crate::config::AppConfig, String>;
}

#[derive(Clone)]
pub struct ConfigHandle(pub Arc<dyn ConfigApi>);

impl std::ops::Deref for ConfigHandle {
    type Target = dyn ConfigApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
