use thiserror::Error;

use crate::key::Key;

#[derive(Debug, Error)]
pub enum Error {
    #[error("service `{0}` is not registered")]
    MissingService(Key),
    #[error("plugin `{0}` is already loaded")]
    DuplicatePlugin(String),
    #[error("plugin `{0}` is not loaded")]
    UnknownPlugin(String),
    #[error("service `{key}` is already provided by `{provider}`")]
    ServiceConflict { key: Key, provider: String },
    #[error("channel `{key}` is already bound to a different event type")]
    EventConflict { key: Key },
    #[error("plugin `{0}` is currently loading")]
    PluginBusy(String),
    #[error("emitted wrong payload type for channel `{key}`")]
    PayloadTypeMismatch { key: Key },
    #[error("plugin `{0}` cannot provide and inject the same service")]
    SelfDependency(String),
    #[error("plugin `{0}` panicked during build: {1}")]
    PluginPanicked(String, String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
