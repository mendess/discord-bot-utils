pub mod event_handler;
pub mod events;

pub use daemons::ControlFlow;
use futures::future::BoxFuture;
use serenity::{
    client::Context,
    prelude::{Mutex, RwLock},
};
use std::{
    any::{type_name, Any, TypeId},
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, OnceLock},
};

type Argument = dyn Any + Send + Sync;

type Callback = dyn for<'args> FnMut(&'args Context, &'args Argument) -> BoxFuture<'args, ControlFlow>
    + Sync
    + Send;

type Subscribers = Vec<Box<Callback>>;

pub trait Event: Any {
    type Argument: Any + Send + Sync;
}

static EVENT_HANDLERS: OnceLock<RwLock<HashMap<TypeId, Arc<Mutex<Subscribers>>>>> = OnceLock::new();

/// Subscribe to an event of type `T`. See [events].
pub async fn subscribe<T, F>(mut f: F)
where
    T: Event,
    F: for<'args> FnMut(&'args Context, &'args T::Argument) -> BoxFuture<'args, ControlFlow>
        + Sync
        + Send
        + 'static,
{
    log::info!(
        "Registered a callback for {}: {}",
        type_name::<T>().split("::").last().unwrap(),
        type_name::<F>()
    );
    let callback: Box<Callback> = Box::new(move |ctx: &Context, any: &Argument| {
        f(ctx, any.downcast_ref::<T::Argument>().unwrap())
    });
    match EVENT_HANDLERS
        .get_or_init(Default::default)
        .write()
        .await
        .entry(TypeId::of::<T>())
    {
        Entry::Occupied(subs) => subs.get().lock().await.push(callback),
        Entry::Vacant(subs) => {
            subs.insert(Arc::new(Mutex::new(vec![callback])));
        }
    };
}

pub(crate) async fn publish_with<T: Event>(ctx: Context, arg: impl FnOnce() -> T::Argument) {
    let mut to_remove = vec![];
    let subscribers = EVENT_HANDLERS
        .get_or_init(Default::default)
        .read()
        .await
        .get(&TypeId::of::<T>())
        .cloned();
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

pub(crate) async fn publish<T>(ctx: Context, arg: T::Argument)
where
    T: Event,
{
    publish_with::<T>(ctx, || arg).await
}
