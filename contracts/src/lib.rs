//! Shared vocabulary for the harness plugin suite.
//!
//! This crate holds the cross-plugin contracts only: domain types, event
//! payloads, and the string keys for services and event channels. Concrete
//! service structs live in the plugin crates that provide them, so the crate
//! graph stays acyclic: every plugin depends on `harness-contracts`, never on
//! another plugin, and communicates everything else through the core bus.

pub mod keys;
pub mod types;

pub use keys::*;
pub use types::*;
