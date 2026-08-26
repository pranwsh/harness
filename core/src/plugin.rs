use crate::{Result, context::Context, key::Key};

#[derive(Debug, Clone, Default)]
pub struct PluginMeta {
    pub(crate) name: String,
    pub(crate) provides: Vec<Key>,
    pub(crate) injects: Vec<Key>,
}

impl PluginMeta {
    pub fn new(name: impl Into<String>) -> Self {
        PluginMeta {
            name: name.into(),
            provides: Vec::new(),
            injects: Vec::new(),
        }
    }

    pub fn provides(mut self, key: impl Into<Key>) -> Self {
        self.provides.push(key.into());
        self
    }

    pub fn injects(mut self, key: impl Into<Key>) -> Self {
        self.injects.push(key.into());
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;

    fn build(&self, ctx: Context) -> Result<()>;
}
