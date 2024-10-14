use core::fmt;
use std::{
    any::type_name,
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    future::Future,
    hash::Hash,
    io,
    marker::PhantomData,
    ops::Deref,
    os::unix::prelude::OsStrExt,
    path::PathBuf,
    str::{from_utf8, FromStr},
};

use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    fs,
    sync::{MappedMutexGuard, Mutex, MutexGuard, OnceCell},
};

use crate::{Database, Deserializer, Serializer};

type SerializerProducer<T, E> = fn() -> Serializer<T, E>;

type DeserializerProducer<T, E> = fn() -> Deserializer<T, E>;

pub trait FileKeySerializer<Pk> {
    type ParseError;
    fn to_string(pk: Pk) -> String;
    fn from_str(s: &str) -> Result<Pk, Self::ParseError>;
}

impl<Pk> FileKeySerializer<Pk> for Pk
where
    Pk: fmt::Display,
    Pk: FromStr<Err: std::error::Error + Send + Sync + 'static>,
{
    type ParseError = io::Error;

    fn to_string(pk: Pk) -> String {
        format!("{pk}.json")
    }

    fn from_str(s: &str) -> Result<Pk, Self::ParseError> {
        s.strip_suffix(".json")
            .ok_or_else(|| io::Error::other(format!("not json: {s}")))?
            .parse()
            .map_err(io::Error::other)
    }
}

pub struct MultifileDb<Pk, T, FK = Pk, E: std::error::Error = io::Error> {
    dir: PathBuf,
    table: OnceCell<Mutex<HashMap<Pk, Database<T, E>>>>,
    new_serializer: SerializerProducer<T, E>,
    new_deserializer: DeserializerProducer<T, E>,
    _marker: PhantomData<FK>,
}

impl<Pk, T> MultifileDb<Pk, T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    pub fn new(base_dir: PathBuf) -> Self {
        Self::new_with_ser_and_deser(
            base_dir,
            || Box::new(|w, t| serde_json::to_writer(w, t).map_err(Into::into)),
            || Box::new(|s| serde_json::from_slice(s).map_err(Into::into)),
        )
    }
}

impl<Pk, T, Fk, E> MultifileDb<Pk, T, Fk, E>
where
    E: From<io::Error> + std::error::Error + 'static,
    T: 'static,
{
    pub fn new_with_ser_and_deser<P>(
        base_dir: P,
        new_serializer: SerializerProducer<T, E>,
        new_deserializer: DeserializerProducer<T, E>,
    ) -> Self
    where
        P: Into<PathBuf>,
    {
        let base_dir = base_dir.into();
        Self {
            dir: base_dir,
            table: OnceCell::default(),
            new_serializer,
            new_deserializer,
            _marker: PhantomData,
        }
    }
}

pub struct DatabaseGuard<'s, T, E: std::error::Error> {
    guard: MappedMutexGuard<'s, Database<T, E>>,
}

impl<T, E: std::error::Error> Deref for DatabaseGuard<'_, T, E> {
    type Target = Database<T, E>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<Pk, T, Fk, E> MultifileDb<Pk, T, Fk, E>
where
    Pk: Eq + Hash + Copy + Debug,
    Fk: FileKeySerializer<Pk>,
    T: 'static,
    E: From<io::Error> + From<Fk::ParseError> + std::error::Error + 'static,
{
    pub async fn get(&self, pk: &Pk) -> Result<Option<DatabaseGuard<'_, T, E>>, E> {
        let map = self.load().await?;
        let lock = map.lock().await;
        if !lock.contains_key(pk) {
            return Ok(None);
        }
        let lock = MutexGuard::map(lock, |g| g.get_mut(pk).unwrap());
        Ok(Some(DatabaseGuard { guard: lock }))
    }

    pub async fn get_or_default(&self, pk: Pk) -> Result<DatabaseGuard<'_, T, E>, E> {
        let map = self.load().await?;
        let mut lock = map.lock().await;
        match lock.entry(pk) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(o) => {
                let mut path = self.dir.clone();
                path.push(Fk::to_string(pk));
                let db = Database::with_ser_and_deser(
                    path,
                    (self.new_serializer)(),
                    (self.new_deserializer)(),
                )
                .await?;
                o.insert(db)
            }
        };
        let lock = MutexGuard::map(lock, |g| g.get_mut(&pk).unwrap());
        Ok(DatabaseGuard { guard: lock })
    }

    pub async fn for_each<A, F, Fut>(&self, mut action: F, args: A) -> io::Result<()>
    where
        F: FnMut(Pk, &Database<T, E>, A) -> Fut,
        A: Copy,
        Fut: Future<Output = ()>,
    {
        let table = self.load().await?;
        for (pk, db) in table.lock().await.iter() {
            action(*pk, db, args).await;
        }
        Ok(())
    }

    pub async fn try_for_each<F, Fut>(&self, mut action: F) -> Result<(), E>
    where
        for<'s> F: FnMut(&'s Pk, &'s Database<T, E>) -> Fut + 's,
        Fut: Future<Output = Result<(), E>>,
    {
        let table = self.load().await?;
        for (pk, db) in table.lock().await.iter() {
            action(pk, db).await?;
        }
        Ok(())
    }

    async fn load(&self) -> io::Result<&Mutex<HashMap<Pk, Database<T, E>>>> {
        self.table
            .get_or_try_init(|| async {
                match fs::read_dir(&self.dir).await {
                    Ok(mut read_dir) => {
                        let mut map = HashMap::default();
                        while let Some(d) = read_dir.next_entry().await? {
                            let path = d.path();
                            let key = path
                                .file_name()
                                .and_then(|n| from_utf8(n.as_bytes()).ok())
                                .and_then(|n| Fk::from_str(n).ok());
                            let gid = match key {
                                None => continue,
                                Some(gid) => gid,
                            };
                            log::debug!(
                                "loaded {gid:?} -> {path:?} into db {}",
                                type_name::<Self>()
                            );
                            map.insert(
                                gid,
                                Database::with_ser_and_deser(
                                    path,
                                    (self.new_serializer)(),
                                    (self.new_deserializer)(),
                                )
                                .await?,
                            );
                        }

                        Ok(Mutex::new(map))
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Default::default()),
                    Err(e) => Err(e),
                }
            })
            .await
    }
}

impl<Pk, T, Fk, E> MultifileDb<Pk, T, Fk, E>
where
    Pk: Eq + Hash + Copy + Debug,
    Fk: FileKeySerializer<Pk>,
    T: 'static,
    E: From<io::Error> + From<Fk::ParseError> + std::error::Error + 'static,
{
    pub async fn iter_guard(&self) -> io::Result<IterGuard<'_, Pk, T, E>> {
        let table = self.load().await?;
        let guard = table.lock().await;
        Ok(IterGuard { _guard: guard })
    }
}

pub struct IterGuard<'s, Pk, T, E>
where
    E: std::error::Error,
{
    _guard: MutexGuard<'s, HashMap<Pk, Database<T, E>>>,
}

impl<'s, Pk, T, E: std::error::Error> IterGuard<'s, Pk, T, E> {
    pub fn iter(&self) -> impl Iterator<Item = (&Pk, &Database<T, E>)> {
        self._guard.iter()
    }
}
