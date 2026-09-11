//! Shared vocabulary for the harness plugin suite.
//!
//! This crate holds the cross-plugin contracts only: domain types, event
//! payloads, string keys, and service traits with sized `Handle` wrappers.
//! Concrete service structs live in the plugin crates that provide them, so
//! the crate graph stays acyclic: every domain plugin depends on
//! `harness-contracts` (and `harness-core` for bus/DI), never on another
//! plugin, and communicates through trait handles under `*_API` / store keys
//! or the core bus. Legacy concrete keys (`sessions.log`, `prompt.assembler`,
//! …) remain alongside the `*_API` handles for backward compat (`model.chat`
//! retired — use `model.streamer`).
//!
//! Exceptions (both acyclic, downward-only, documented as intentional
//! coupling):
//! - TUI leaf shell (`tui`, `tui-model`, `tui-input`, `tui-markdown`,
//!   `tui-popup`, `tui-state`): the shell composes concrete TUI/loop crates
//!   it owns; e.g. `tui` → `harness-agent-loop` + `harness-tui-state`.
//! - Hash hashing infra (`hashline-read`/`hashline-edit` → `hash-base`):
//!   shared file-hashing library (pure line hashing + in-memory `HashStore`),
//!   reused the way `tui-state` is reused. Prefer a `HashStoreApi` trait
//!   handle if this surface ever widens.
//! Domain plugins must never depend on TUI, and TUI never feeds domain
//! services back into providers.

pub mod config;
pub mod keys;
pub mod services;
pub mod types;

pub use config::{
    AgentConfig, AppConfig, ConfigError, LlmConfig, ShellConfig, default_max_iterations,
};
pub use keys::*;
pub use services::*;
pub use types::*;
