use std::any::TypeId;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use crate::{
    key::Key,
    plugin::{Plugin, PluginMeta},
};

pub(crate) struct Node {
    pub(crate) meta: PluginMeta,
}

#[derive(Default)]
pub(crate) struct DepGraph {
    active: HashMap<String, Node>,
    building: HashSet<String>,
    order: Vec<String>,
    provider_of: HashMap<Key, String>,
    pending: Vec<Arc<dyn Plugin>>,
    pending_index: HashSet<String>,
    declared_emits: HashMap<Key, Vec<(String, TypeId)>>,
    declared_listens: HashMap<Key, Vec<(String, TypeId)>>,
}

impl DepGraph {
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.active.contains_key(name) || self.pending_index.contains(name)
    }

    pub(crate) fn provider_of(&self, key: &Key) -> Option<&str> {
        self.provider_of.get(key).map(String::as_str)
    }

    pub(crate) fn missing_deps(
        &self,
        injects: &[Key],
        svc_available: impl Fn(&Key) -> bool,
    ) -> Vec<Key> {
        injects
            .iter()
            .filter(|k| !svc_available(k))
            .cloned()
            .collect()
    }

    pub(crate) fn push_pending(&mut self, plugin: Arc<dyn Plugin>) {
        let meta = plugin.meta();
        self.pending_index.insert(meta.name);
        self.pending.push(plugin);
    }

    pub(crate) fn remove_pending(&mut self, name: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.meta().name() != name);
        if self.pending.len() != before {
            self.pending_index.remove(name);
            true
        } else {
            false
        }
    }

    pub(crate) fn take_satisfied(
        &mut self,
        svc_available: impl Fn(&Key) -> bool,
    ) -> Vec<Arc<dyn Plugin>> {
        let candidates = std::mem::take(&mut self.pending);
        let mut ready = Vec::new();
        let mut kept = Vec::with_capacity(candidates.len());
        for plugin in candidates {
            let meta = plugin.meta();
            if meta.injects.iter().all(&svc_available) {
                self.pending_index.remove(&meta.name);
                ready.push(plugin);
            } else {
                kept.push(plugin);
            }
        }
        self.pending = kept;
        ready
    }

    pub(crate) fn is_building(&self, name: &str) -> bool {
        self.building.contains(name)
    }

    pub(crate) fn reserve(&mut self, meta: PluginMeta) {
        let name = meta.name.clone();
        for key in &meta.provides {
            self.provider_of.insert(key.clone(), name.clone());
        }
        for (key, ty) in &meta.emits {
            self.declared_emits
                .entry(key.clone())
                .or_default()
                .push((name.clone(), *ty));
        }
        for (key, ty) in &meta.listens {
            self.declared_listens
                .entry(key.clone())
                .or_default()
                .push((name.clone(), *ty));
        }
        self.active.insert(name.clone(), Node { meta });
        self.order.push(name.clone());
        self.building.insert(name);
    }

    pub(crate) fn promote(&mut self, name: &str) {
        self.building.remove(name);
    }

    pub(crate) fn rollback(&mut self, name: &str) -> Option<PluginMeta> {
        if !self.building.remove(name) {
            return None;
        }
        self.remove_active(name)
    }

    pub(crate) fn remove_active(&mut self, name: &str) -> Option<PluginMeta> {
        let node = self.active.remove(name)?;
        self.order.retain(|n| n != name);
        for key in &node.meta.provides {
            debug_assert!(
                self.provider_of.get(key).is_none_or(|p| p == name),
                "service `{key}` was provided by `{name}`"
            );
            self.provider_of.remove(key);
        }
        for entries in self.declared_emits.values_mut() {
            entries.retain(|(owner, _)| owner != name);
        }
        for entries in self.declared_listens.values_mut() {
            entries.retain(|(owner, _)| owner != name);
        }
        self.declared_emits.retain(|_, e| !e.is_empty());
        self.declared_listens.retain(|_, e| !e.is_empty());
        Some(node.meta)
    }

    pub(crate) fn conflicting_event_decl(&self, key: &Key, ty: TypeId) -> Option<String> {
        let find = |map: &HashMap<Key, Vec<(String, TypeId)>>| {
            map.get(key)?
                .iter()
                .find_map(|(owner, t)| (*t != ty).then_some(owner.clone()))
        };
        find(&self.declared_emits).or_else(|| find(&self.declared_listens))
    }

    pub(crate) fn emitters_of(&self, key: &Key) -> Vec<String> {
        self.declared_emits
            .get(key)
            .map(|v| v.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn listeners_of(&self, key: &Key) -> Vec<String> {
        self.declared_listens
            .get(key)
            .map(|v| v.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn teardown_order(&self, name: &str) -> Vec<String> {
        let pos: HashMap<&str, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let Some(start) = pos.get(name).copied() else {
            return Vec::new();
        };

        // Dependency edges between active plugins: d -> p means d injects a
        // key provided by p. Deduped per pair; degrees are tiny so a linear
        // scan suffices.
        let mut fwd: Vec<Vec<usize>> = vec![Vec::new(); self.order.len()];
        let mut rev: Vec<Vec<usize>> = vec![Vec::new(); self.order.len()];
        for (d, dep) in self.order.iter().enumerate() {
            let node = &self.active[dep];
            for key in &node.meta.injects {
                let Some(provider) = self.provider_of.get(key) else {
                    continue;
                };
                if let Some(&p) = pos.get(provider.as_str())
                    && p != d
                    && !fwd[d].contains(&p)
                {
                    fwd[d].push(p);
                    rev[p].push(d);
                }
            }
        }

        // Affected set: every transitive dependent of `start`, excluding it.
        let mut affected: HashSet<usize> = HashSet::new();
        let mut stack: Vec<usize> = rev[start].clone();
        while let Some(d) = stack.pop() {
            if affected.insert(d) {
                stack.extend(rev[d].iter().copied());
            }
        }

        // Kahn over the affected sub-DAG: emit a node once every in-set
        // dependent has been emitted, breaking ties by lowest load order.
        let mut indegree: Vec<usize> = vec![0; self.order.len()];
        for &d in &affected {
            for &p in &fwd[d] {
                if affected.contains(&p) {
                    indegree[p] += 1;
                }
            }
        }
        let mut ready: BinaryHeap<Reverse<usize>> = affected
            .iter()
            .filter(|&&d| indegree[d] == 0)
            .map(|&d| Reverse(d))
            .collect();

        let mut out: Vec<String> = Vec::with_capacity(affected.len());
        while let Some(Reverse(d)) = ready.pop() {
            out.push(self.order[d].clone());
            for &p in &fwd[d] {
                if affected.contains(&p) {
                    indegree[p] -= 1;
                    if indegree[p] == 0 {
                        ready.push(Reverse(p));
                    }
                }
            }
        }
        out
    }

    pub(crate) fn active_names(&self) -> Vec<String> {
        self.order.clone()
    }

    pub(crate) fn pending_names(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(|p| p.meta().name.to_owned())
            .collect()
    }
}
