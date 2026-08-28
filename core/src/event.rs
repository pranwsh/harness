use std::{
    any::{Any, TypeId},
    collections::HashMap,
    marker::PhantomData,
    sync::Arc,
};

use futures::future::BoxFuture;

use crate::{context::Context, context::Owner, error::Error, key::Key};

pub trait Event: Any + Send + Sync {}

impl<E: Any + Send + Sync> Event for E {}

pub type BoxedEvent = Arc<dyn Event>;

pub type HandlerFuture = BoxFuture<'static, ()>;

pub type Handler = dyn Fn(BoxedEvent) -> HandlerFuture + Send + Sync;

pub type WaterfallFuture = BoxFuture<'static, BoxedEvent>;

pub type WaterfallHandler = dyn Fn(BoxedEvent) -> WaterfallFuture + Send + Sync;

pub(crate) enum ListenerKind {
    Sync(Arc<dyn Fn(BoxedEvent) + Send + Sync>),
    Async(Arc<Handler>),
    Waterfall(Arc<WaterfallHandler>),
}

pub(crate) struct ListenerEntry {
    pub(crate) owner: Owner,
    pub(crate) kind: ListenerKind,
}

#[derive(Default, Clone)]
pub(crate) struct EventRegistry {
    channels: HashMap<Key, Channel>,
}

#[derive(Clone)]
pub(crate) struct Channel {
    pub(crate) ty: TypeId,
    pub(crate) listeners: Vec<Arc<ListenerEntry>>,
}

impl EventRegistry {
    pub(crate) fn add(
        &mut self,
        key: Key,
        ty: TypeId,
        owner: Owner,
        kind: ListenerKind,
    ) -> Result<(), Error> {
        match self.channels.get_mut(&key) {
            Some(channel) => {
                if channel.ty != ty {
                    return Err(Error::EventConflict { key });
                }
                channel
                    .listeners
                    .push(Arc::new(ListenerEntry { owner, kind }));
            }
            None => {
                self.channels.insert(
                    key,
                    Channel {
                        ty,
                        listeners: vec![Arc::new(ListenerEntry { owner, kind })],
                    },
                );
            }
        }
        Ok(())
    }

    pub(crate) fn channel(&self, key: &Key) -> Option<&Channel> {
        self.channels.get(key)
    }

    pub(crate) fn remove_owners(&mut self, owners: &[Owner]) {
        self.channels.retain(|_, channel| {
            channel
                .listeners
                .retain(|entry| !owners.contains(&entry.owner));
            !channel.listeners.is_empty()
        });
    }
}

pub(crate) fn downcast_event<E: Event>(ev: BoxedEvent) -> Arc<E> {
    let any: Arc<dyn Any + Send + Sync> = ev;
    any.downcast::<E>().expect("event payload type mismatch")
}

pub struct Events<E: Event> {
    ctx: Context,
    key: Key,
    _marker: PhantomData<fn() -> E>,
}

impl<E: Event> Events<E> {
    pub(crate) fn new(ctx: Context, key: impl Into<Key>) -> Self {
        Events {
            ctx,
            key: key.into(),
            _marker: PhantomData,
        }
    }

    pub fn on<F, Fut>(&self, handler: F) -> Result<(), Error>
    where
        F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.ctx.on_key::<E, F, Fut>(self.key.clone(), handler)
    }

    pub fn emit(&self, event: E) -> Result<Vec<HandlerFuture>, Error> {
        self.ctx.emit_key(self.key.clone(), event)
    }

    #[cfg(feature = "rt-tokio")]
    pub fn emit_detached(&self, event: E) -> Result<(), Error> {
        self.ctx.emit_key_detached(self.key.clone(), event)
    }

    pub fn on_waterfall<F, Fut>(&self, handler: F) -> Result<(), Error>
    where
        F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = E> + Send + 'static,
    {
        self.ctx
            .on_waterfall_key::<E, F, Fut>(self.key.clone(), handler)
    }

    pub async fn waterfall(&self, event: E) -> Result<E, Error>
    where
        E: Clone,
    {
        self.ctx.waterfall_key(self.key.clone(), event).await
    }
}
