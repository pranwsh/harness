use std::{
    any::{Any, TypeId},
    collections::HashMap,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use crate::{
    error::{Error, Result},
    event::{
        BoxedEvent, Event, EventRegistry, Events, Handler, HandlerFuture, ListenerEntry,
        ListenerKind, downcast_event,
    },
    graph::DepGraph,
    key::Key,
    plugin::{Plugin, PluginMeta},
    service::ServiceRegistry,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Owner {
    System,
    Plugin(String),
}

#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Arc<Inner>,
    pub(crate) owner: Owner,
}

pub(crate) struct Inner {
    services: RwLock<ServiceRegistry>,
    events: RwLock<EventRegistry>,
    graph: Mutex<DepGraph>,
}

impl Inner {
    fn lock_graph(&self) -> MutexGuard<'_, DepGraph> {
        self.graph.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_services(&self) -> RwLockReadGuard<'_, ServiceRegistry> {
        self.services.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_events(&self) -> RwLockReadGuard<'_, EventRegistry> {
        self.events.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_services(&self) -> RwLockWriteGuard<'_, ServiceRegistry> {
        self.services
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write_events(&self) -> RwLockWriteGuard<'_, EventRegistry> {
        self.events.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    Activated,
    Pending { missing: Vec<Key> },
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(rest) => match rest.downcast::<&'static str>() {
            Ok(s) => (*s).to_owned(),
            Err(_) => "non-string panic payload".to_owned(),
        },
    }
}

impl Context {
    pub fn root() -> Self {
        Self {
            inner: Arc::new(Inner {
                services: RwLock::new(ServiceRegistry::default()),
                events: RwLock::new(EventRegistry::default()),
                graph: Mutex::new(DepGraph::default()),
            }),
            owner: Owner::System,
        }
    }

    pub fn provide_key<T: Send + Sync + 'static>(&self, key: impl Into<Key>, svc: Arc<T>) {
        let svc: Arc<dyn std::any::Any + Send + Sync> = svc;
        let key = key.into();
        self.modify_services(|reg| reg.insert(key, svc));
    }

    pub fn inject_key<T: Send + Sync + 'static>(&self, key: impl Into<Key>) -> Result<Arc<T>> {
        let key = key.into();
        self.inner
            .read_services()
            .get::<T>(&key)
            .ok_or(Error::MissingService(key))
    }

    pub fn try_inject_key<T: Send + Sync + 'static>(&self, key: impl Into<Key>) -> Option<Arc<T>> {
        self.inner.read_services().get::<T>(&key.into())
    }

    pub fn on_key<E, F, Fut>(&self, key: impl Into<Key>, handler: F) -> Result<()>
    where
        E: Event,
        F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let raw: Arc<Handler> = Arc::new(move |ev| {
            let e = downcast_event::<E>(ev);
            Box::pin(handler(e))
        });
        self.add_raw_listener::<E>(key, ListenerKind::Async(raw))
    }

    pub fn on_sync_key<E, F>(&self, key: impl Into<Key>, handler: F) -> Result<()>
    where
        E: Event,
        F: Fn(&E) + Send + Sync + 'static,
    {
        let raw: Arc<dyn Fn(BoxedEvent) + Send + Sync> = Arc::new(move |ev| {
            let e = downcast_event::<E>(ev);
            handler(&e);
        });
        self.add_raw_listener::<E>(key, ListenerKind::Sync(raw))
    }

    /// Emits an event on the given channel.
    ///
    /// The listener list is snapshotted under the registry read lock and then
    /// released before any handler runs, so handlers may freely register or
    /// remove listeners (including on this channel) without deadlocking.
    ///
    /// Sync listeners (`on_sync_key`) run inline on the calling thread before
    /// this returns; a slow sync handler will stall the emitter and any other
    /// listeners. Async listeners are not awaited — their futures are returned
    /// so the caller can drive them (see `emit_key_detached` to spawn instead).
    ///
    /// Emitting on a channel with no listeners — including one that was never
    /// created — is a silent no-op that allocates nothing.
    pub fn emit_key<E: Event>(&self, key: impl Into<Key>, event: E) -> Result<Vec<HandlerFuture>> {
        let key = key.into();
        let listeners: Vec<Arc<ListenerEntry>> = {
            let reg = self.inner.read_events();
            let Some(channel) = reg.channel(&key) else {
                return Ok(Vec::new());
            };
            if channel.ty != TypeId::of::<E>() {
                return Err(Error::PayloadTypeMismatch { key });
            }
            if channel.listeners.is_empty() {
                return Ok(Vec::new());
            }
            channel.listeners.clone()
        };

        let ev: BoxedEvent = Arc::new(event);
        let mut pending = Vec::new();
        for entry in listeners {
            match &entry.kind {
                ListenerKind::Sync(f) => f(ev.clone()),
                ListenerKind::Async(h) => pending.push(h(ev.clone())),
            }
        }
        Ok(pending)
    }

    #[cfg(feature = "rt-tokio")]
    pub fn emit_key_detached<E: Event>(&self, key: impl Into<Key>, event: E) -> Result<()> {
        for fut in self.emit_key(key, event)? {
            tokio::spawn(fut);
        }
        Ok(())
    }

    pub fn events<E: Event>(&self, key: impl Into<Key>) -> Events<E> {
        Events::new(self.clone(), key)
    }

    pub fn load<P: Plugin>(&self, plugin: P) -> Result<LoadOutcome> {
        self.load_dyn(Arc::new(plugin))
    }

    pub fn load_dyn(&self, plugin: Arc<dyn Plugin>) -> Result<LoadOutcome> {
        let meta = plugin.meta();

        // Lock order is always `graph` then `services`; never the reverse.
        {
            let mut graph = self.inner.lock_graph();
            let reg = self.inner.read_services();

            // Lock order extends to `events`, always acquired last.
            let events = self.inner.read_events();
            self.validate_meta(&graph, &reg, &events, &meta)?;

            let missing = graph.missing_deps(&meta.injects, |k| reg.contains(k));
            if !missing.is_empty() {
                graph.push_pending(plugin);
                return Ok(LoadOutcome::Pending { missing });
            }
            graph.reserve(meta);
        }

        self.build_reserved(plugin)?;
        self.activate_pending()?;
        Ok(LoadOutcome::Activated)
    }

    fn validate_meta(
        &self,
        graph: &DepGraph,
        services: &ServiceRegistry,
        events: &EventRegistry,
        meta: &PluginMeta,
    ) -> Result<()> {
        let name = meta.name().to_owned();
        if graph.contains(meta.name()) {
            return Err(Error::DuplicatePlugin(name));
        }
        if meta.provides.iter().any(|k| meta.injects.contains(k)) {
            return Err(Error::SelfDependency(name));
        }
        for key in &meta.provides {
            if let Some(provider) = graph.provider_of(key) {
                return Err(Error::ServiceConflict {
                    key: key.clone(),
                    provider: provider.to_owned(),
                });
            }
            if services.contains(key) {
                return Err(Error::ServiceConflict {
                    key: key.clone(),
                    provider: "system".to_owned(),
                });
            }
        }
        let mut declared: HashMap<&Key, TypeId> = HashMap::new();
        for (key, ty) in meta.emits.iter().chain(&meta.listens) {
            match declared.get(key) {
                Some(prev) if *prev != *ty => {
                    return Err(Error::SelfEventConflict {
                        plugin: name.clone(),
                        key: key.clone(),
                    });
                }
                _ => {
                    declared.insert(key, *ty);
                }
            }
            if let Some(other) = graph.conflicting_event_decl(key, *ty) {
                return Err(Error::EventDeclConflict {
                    plugin: name.clone(),
                    key: key.clone(),
                    other,
                });
            }
            if let Some(channel) = events.channel(key)
                && channel.ty != *ty
            {
                return Err(Error::EventChannelMismatch {
                    plugin: name.clone(),
                    key: key.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn unload(&self, name: &str) -> Result<Vec<String>> {
        // Hold the graph lock for the whole teardown decision so no concurrent
        // load can observe half-removed state; release it (via block scope)
        // before touching the service/event registries.
        let (order, removed): (Vec<String>, Vec<PluginMeta>) = {
            let mut graph = self.inner.lock_graph();
            if !graph.contains(name) {
                return Err(Error::UnknownPlugin(name.to_owned()));
            }
            if graph.remove_pending(name) {
                return Ok(Vec::new());
            }
            let order = graph.teardown_order(name);
            if graph.is_building(name) || order.iter().any(|dep| graph.is_building(dep)) {
                return Err(Error::PluginBusy(name.to_owned()));
            }
            let mut removed = Vec::with_capacity(order.len() + 1);
            for dep in &order {
                removed.push(graph.remove_active(dep));
            }
            removed.push(graph.remove_active(name));
            (order, removed.into_iter().flatten().collect())
        };

        let service_keys: Vec<Key> = removed
            .iter()
            .flat_map(|meta| meta.provides.iter().cloned())
            .collect();
        let owners: Vec<Owner> = removed
            .into_iter()
            .map(|meta| Owner::Plugin(meta.name))
            .collect();

        self.modify_services(|reg| {
            for key in &service_keys {
                reg.remove(key);
            }
        });
        self.modify_events(|reg| reg.remove_owners(&owners));
        Ok(order)
    }

    pub fn plugin_names(&self) -> Vec<String> {
        self.inner.lock_graph().active_names()
    }

    pub fn pending_names(&self) -> Vec<String> {
        self.inner.lock_graph().pending_names()
    }

    pub fn provider_of(&self, key: &Key) -> Option<String> {
        self.inner.lock_graph().provider_of(key).map(str::to_owned)
    }

    pub fn emitters_of(&self, key: &Key) -> Vec<String> {
        self.inner.lock_graph().emitters_of(key)
    }

    pub fn listeners_of(&self, key: &Key) -> Vec<String> {
        self.inner.lock_graph().listeners_of(key)
    }

    fn build_reserved(&self, plugin: Arc<dyn Plugin>) -> Result<()> {
        let name = plugin.meta().name().to_owned();
        let owner = Owner::Plugin(name.clone());

        let built = match catch_unwind(AssertUnwindSafe(|| {
            plugin.build(Context {
                inner: self.inner.clone(),
                owner,
            })
        })) {
            Ok(built) => built,
            Err(payload) => {
                self.inner.lock_graph().rollback(&name);
                return Err(Error::PluginPanicked(name, panic_message(payload)));
            }
        };

        if built.is_ok() {
            self.inner.lock_graph().promote(&name);
        } else {
            self.inner.lock_graph().rollback(&name);
        }
        built
    }

    fn activate_pending(&self) -> Result<()> {
        loop {
            let ready = {
                let mut graph = self.inner.lock_graph();
                let reg = self.inner.read_services();
                graph.take_satisfied(|k| reg.contains(k))
            };
            if ready.is_empty() {
                break;
            }
            let mut first_err = None;
            for plugin in ready {
                let outcome = {
                    let mut graph = self.inner.lock_graph();
                    let reg = self.inner.read_services();

                    // Lock order extends to `events`, always acquired last.
                    let events = self.inner.read_events();
                    let meta = plugin.meta();
                    match self.validate_meta(&graph, &reg, &events, &meta) {
                        Ok(()) => {
                            graph.reserve(meta);
                            None
                        }
                        Err(err) => Some(err),
                    }
                };
                match outcome {
                    None => {
                        if let Err(err) = self.build_reserved(plugin) {
                            first_err.get_or_insert(err);
                        }
                    }
                    Some(err) => {
                        first_err.get_or_insert(err);
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
        }
        Ok(())
    }

    fn add_raw_listener<E: Event>(&self, key: impl Into<Key>, kind: ListenerKind) -> Result<()> {
        let owner = self.owner.clone();
        let key = key.into();
        let ty = TypeId::of::<E>();
        self.modify_events(move |reg| reg.add(key, ty, owner, kind))
    }

    fn modify_services<T>(&self, f: impl FnOnce(&mut ServiceRegistry) -> T) -> T {
        f(&mut self.inner.write_services())
    }

    fn modify_events<T>(&self, f: impl FnOnce(&mut EventRegistry) -> T) -> T {
        f(&mut self.inner.write_events())
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("owner", &self.owner)
            .finish()
    }
}
