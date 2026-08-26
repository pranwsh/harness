use std::{any::Any, collections::HashMap, sync::Arc};

use crate::key::Key;

#[derive(Default, Clone)]
pub(crate) struct ServiceRegistry {
    map: HashMap<Key, Arc<dyn Any + Send + Sync>>,
}

impl ServiceRegistry {
    pub(crate) fn insert(&mut self, key: Key, svc: Arc<dyn Any + Send + Sync>) {
        self.map.insert(key, svc);
    }

    pub(crate) fn remove(&mut self, key: &Key) {
        self.map.remove(key);
    }

    pub(crate) fn contains(&self, key: &Key) -> bool {
        self.map.contains_key(key)
    }

    pub(crate) fn get<T: Send + Sync + 'static>(&self, key: &Key) -> Option<Arc<T>> {
        self.map.get(key)?.clone().downcast::<T>().ok()
    }
}
