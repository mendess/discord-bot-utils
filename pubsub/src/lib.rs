pub mod event_handler;
pub mod events;

pub use daemons::ControlFlow;
use futures::future::BoxFuture;
use serenity::{client::Context, prelude::Mutex};
use std::any::{type_name, Any};

pub(crate) type Callback<A> =
    dyn for<'args> FnMut(&'args Context, &'args A) -> BoxFuture<'args, ControlFlow> + Sync + Send;

pub trait Event {
    type Argument: Any + Send + Sync;

    fn container() -> &'static Mutex<Vec<Box<Callback<Self::Argument>>>>;
}

#[macro_export]
macro_rules! impl_event_container {
    ($arg:ty) => {
        fn container() -> &'static ::serenity::prelude::Mutex<
            ::std::vec::Vec<::std::boxed::Box<$crate::Callback<$arg>>>,
        > {
            static CONTAINER: ::serenity::prelude::Mutex<
                ::std::vec::Vec<::std::boxed::Box<$crate::Callback<$arg>>>,
            > = ::serenity::prelude::Mutex::const_new(::std::vec::Vec::new());
            &CONTAINER
        }
    };
}

/// Subscribe to an event of type `T`. See [events].
pub async fn subscribe<T, F>(f: F)
where
    T: Event + 'static,
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
    T::container().lock().await.push(Box::new(f));
}

pub(crate) async fn publish_with<T: Event + 'static>(
    ctx: Context,
    arg: impl FnOnce() -> T::Argument,
) {
    let subscribers = T::container();
    let arg = arg();
    tokio::spawn(async move {
        let mut to_remove = vec![];
        let mut subscribers = subscribers.lock().await;
        for (i, s) in subscribers.iter_mut().enumerate() {
            if s(&ctx, &arg).await.is_break() {
                to_remove.push(i);
            }
        }
        for i in to_remove.iter().rev() {
            let _ = subscribers.swap_remove(*i);
            log::trace!("Removed a callback for {}, index: {}", type_name::<T>(), i);
        }
    });
}

pub(crate) async fn publish<T>(ctx: Context, arg: T::Argument)
where
    T: Event + 'static,
{
    publish_with::<T>(ctx, || arg).await
}
