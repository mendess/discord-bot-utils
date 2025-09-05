pub mod event_handler;
pub mod events;

use crate::event_handler::Context;
use futures::future::BoxFuture;
use serenity::prelude::{Mutex, RwLock};
use std::{
    any::{type_name, Any, TypeId},
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, LazyLock},
};

pub type ControlFlow = std::ops::ControlFlow<(), ()>;

type Argument = dyn Any + Send + Sync;

type Callback<S> = dyn for<'args> FnMut(&'args Context<S>, &'args Argument) -> BoxFuture<'args, ControlFlow>
    + Sync
    + Send;

type Subscribers<S> = Vec<Box<Callback<S>>>;

pub trait Event: Any {
    type Argument: Any + Send + Sync;
}

pub struct EventBus<S> {
    #[allow(clippy::type_complexity)]
    handlers: LazyLock<RwLock<HashMap<TypeId, Arc<Mutex<Subscribers<S>>>>>>,
}

impl<S: Send + Sync + 'static> EventBus<S> {
    pub const fn new() -> Self {
        Self {
            handlers: LazyLock::new(Default::default),
        }
    }

    /// Subscribe to an event of type `T`. See [events].
    pub async fn subscribe<T, F>(&self, mut f: F)
    where
        T: Event,
        F: for<'args> FnMut(&'args Context<S>, &'args T::Argument) -> BoxFuture<'args, ControlFlow>
            + Sync
            + Send
            + 'static,
    {
        log::info!(
            "Registered a callback for {}: {}",
            type_name::<T>().split("::").last().unwrap(),
            type_name::<F>()
        );
        let callback: Box<Callback<S>> = Box::new(move |ctx: &Context<S>, any: &Argument| {
            f(ctx, any.downcast_ref::<T::Argument>().unwrap())
        });
        match self.handlers.write().await.entry(TypeId::of::<T>()) {
            Entry::Occupied(subs) => subs.get().lock().await.push(callback),
            Entry::Vacant(subs) => {
                subs.insert(Arc::new(Mutex::new(vec![callback])));
            }
        };
    }

    pub(crate) async fn publish_with<T: Event>(
        &self,
        ctx: Context<S>,
        arg: impl FnOnce() -> T::Argument,
    ) {
        let mut to_remove = vec![];
        let subscribers = self.handlers.read().await.get(&TypeId::of::<T>()).cloned();
        if let Some(subscribers) = subscribers {
            let arg = arg();
            tokio::spawn(async move {
                let mut subscribers = subscribers.lock().await;
                for (i, s) in subscribers.iter_mut().enumerate() {
                    if s(&ctx, &arg).await.is_break() {
                        to_remove.push(i);
                    }
                }
                for i in to_remove.iter().rev() {
                    let _ = subscribers.remove(*i);
                    log::trace!("Removed a callback for {}, index: {}", type_name::<T>(), i);
                }
            });
        }
    }

    pub(crate) async fn publish<T>(&self, ctx: Context<S>, arg: T::Argument)
    where
        T: Event,
    {
        self.publish_with::<T>(ctx, || arg).await
    }
}

impl<S: Send + Sync + 'static> Default for EventBus<S> {
    fn default() -> Self {
        Self::new()
    }
}
