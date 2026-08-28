use std::any::TypeId;

use crate::{Result, context::Context, event::Event, key::Key};

#[derive(Debug, Clone, Default)]
pub struct PluginMeta {
    pub(crate) name: String,
    pub(crate) provides: Vec<Key>,
    pub(crate) injects: Vec<Key>,
    pub(crate) emits: Vec<(Key, TypeId)>,
    pub(crate) listens: Vec<(Key, TypeId)>,
    pub(crate) waterfalls: Vec<(Key, TypeId)>,
}

impl PluginMeta {
    pub fn new(name: impl Into<String>) -> Self {
        PluginMeta {
            name: name.into(),
            provides: Vec::new(),
            injects: Vec::new(),
            emits: Vec::new(),
            listens: Vec::new(),
            waterfalls: Vec::new(),
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

    pub fn emits<E: Event>(mut self, key: impl Into<Key>) -> Self {
        self.emits.push((key.into(), TypeId::of::<E>()));
        self
    }

    pub fn listens<E: Event>(mut self, key: impl Into<Key>) -> Self {
        self.listens.push((key.into(), TypeId::of::<E>()));
        self
    }

    pub fn waterfalls<E: Event>(mut self, key: impl Into<Key>) -> Self {
        self.waterfalls.push((key.into(), TypeId::of::<E>()));
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn emits_of(&self) -> &[(Key, TypeId)] {
        &self.emits
    }

    pub fn listens_of(&self) -> &[(Key, TypeId)] {
        &self.listens
    }

    pub fn waterfalls_of(&self) -> &[(Key, TypeId)] {
        &self.waterfalls
    }
}

pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;

    fn build(&self, ctx: Context) -> Result<()>;
}
