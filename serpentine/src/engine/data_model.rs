//! contains the definitions of the various core node types and structures.

use std::hash::Hash;
use std::sync::Arc;

use crate::engine::cache::{CacheHash, CacheScope, ContentHash};
use crate::engine::filesystem::FileSystem;
use crate::engine::nodes::NodeImpl;
use crate::engine::{RuntimeContext, containerd};
use crate::snek::span::Spanned;

/// Shared field generators for fuzzing.
#[cfg(test)]
pub(crate) mod fuzz {
    use std::sync::Arc;

    use bolero::ValueGenerator as _;

    /// Generator for `Arc<str>` values, which bolero has no built-in generator for.
    pub(crate) fn arc_str() -> impl bolero::ValueGenerator<Output = Arc<str>> {
        bolero::produce::<String>().map_gen(Arc::from)
    }
}

/// Holds the various forms of data that the node engine uses
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub enum Data {
    /// A numeric whole number value
    Int(i128),
    /// A string, usually a short literal
    String(#[cfg_attr(test, generator(fuzz::arc_str()))] Arc<str>),
    /// A docker container
    Container(containerd::ContainerState),
    /// A service
    Service(containerd::ServiceState),
    /// A file/folder
    FileSystem(FileSystem),
}

/// A version of `Data` that contains the data that can be cached.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub enum CacheableData {
    /// `Int`
    Int(i128),
    /// `String`
    String(#[cfg_attr(test, generator(fuzz::arc_str()))] Arc<str>),
    /// `Container`
    Container(containerd::ContainerState),
    /// `Service`
    Service(containerd::ServiceState),
}

/// A value referencing a external data source thats up for cache driven cleanup.
///
/// This is used instead of just cleaning out induvidual `Data` values as multiple data values can
/// reference the same external resource, like a containerd snapshot.
///
/// This type lets us uniformly collect all such keys and ensure we only clean out the ones that
/// arent used.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    /// A containerd snapshot
    Snapshot(Arc<str>),
}

impl Data {
    /// Return the type of this data.
    #[must_use]
    pub fn type_(&self) -> DataType {
        match self {
            Data::Int(_) => DataType::Int,
            Data::String(_) => DataType::String,
            Data::Container(_) => DataType::Container,
            Data::Service(_) => DataType::Service,
            Data::FileSystem(_) => DataType::FileSystem,
        }
    }

    /// Convert this data into a `CacheableData` if it is cacheable, otherwise return `None`.
    #[must_use]
    pub fn cacheable(&self) -> Option<CacheableData> {
        match self {
            Data::Int(value) => Some(CacheableData::Int(*value)),
            Data::String(string) => Some(CacheableData::String(Arc::clone(string))),
            Data::Container(container) => Some(CacheableData::Container(container.clone())),
            Data::Service(service) => Some(CacheableData::Service(service.clone())),
            Data::FileSystem(_) => None,
        }
    }

    /// Construct a `Data` from a `CacheableData`.
    #[must_use]
    pub fn from_cacheable(cacheable_data: CacheableData) -> Self {
        match cacheable_data {
            CacheableData::Int(value) => Data::Int(value),
            CacheableData::String(string) => Data::String(string),
            CacheableData::Container(container) => Data::Container(container),
            CacheableData::Service(service) => Data::Service(service),
        }
    }
}

impl CacheableData {
    /// Retrieve the resource keys for this data, if any.
    pub fn resource_keys(&self) -> Vec<ResourceKey> {
        match self {
            Self::Container(state) => {
                let mut snapshots = Vec::new();
                state.collect_snapshots(&mut snapshots);
                snapshots.into_iter().map(ResourceKey::Snapshot).collect()
            }
            Self::Service(state) => {
                let mut snapshots = Vec::new();
                state.collect_snapshots(&mut snapshots);
                snapshots.into_iter().map(ResourceKey::Snapshot).collect()
            }
            Self::Int(_) | Self::String(_) => Vec::new(),
        }
    }

    /// Check if this data is still valid
    pub async fn healthcheck(&self, ctx: &RuntimeContext) -> bool {
        match self {
            Self::Container(state) => ctx.containerd.healthcheck_value(state).await,
            Self::Service(state) => ctx.containerd.healthcheck_value(state).await,
            Self::Int(_) | Self::String(_) => true,
        }
    }

    /// Export any data pointed to by this value into the blob cache
    pub async fn export_external_data(&self, ctx: &RuntimeContext) {
        match self {
            Self::Container(state) => ctx.containerd.export_snapshots_from(state).await,
            Self::Service(state) => ctx.containerd.export_snapshots_from(state).await,
            Self::Int(_) | Self::String(_) => {}
        }
    }
}

impl ResourceKey {
    /// Return a cache hash for this resource key, must match the cache hash used when trying to
    /// retrive/save the resource to the cache backend.
    pub async fn cache_hash(&self) -> miette::Result<CacheHash> {
        match self {
            Self::Snapshot(name) => CacheHash::from_data(CacheScope::Snapshot, name).await,
        }
    }

    /// Delete this resource from the various backends, if it exists.
    pub async fn clean(self, containerd: &containerd::Client) -> miette::Result<()> {
        match self {
            Self::Snapshot(name) => containerd.delete(&name).await?,
        }

        Ok(())
    }
}

impl ContentHash for Data {
    async fn content_hash(&self, hasher: &mut blake3::Hasher) -> miette::Result<()> {
        hasher.update(&[self.type_() as u8]);

        match self {
            Data::Int(value) => value.content_hash(hasher).await?,
            Data::String(string) => string.content_hash(hasher).await?,
            Data::Container(state) => state.content_hash(hasher).await?,
            Data::Service(state) => state.content_hash(hasher).await?,
            Data::FileSystem(fs) => fs.content_hash(hasher).await?,
        }

        Ok(())
    }
}

/// A companion enum to `Data` denoting the variant/type
#[derive(PartialEq, Eq, Clone, Copy)]
#[repr(u8)]
pub enum DataType {
    /// A integer
    Int,
    /// A string
    String,
    /// A docker container
    Container,
    /// A service
    Service,
    /// A file or folder
    FileSystem,
}

impl DataType {
    /// Return a human friendly version of this type
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Int => "integer",
            Self::String => "string",
            Self::Container => "container",
            Self::Service => "service",
            Self::FileSystem => "file/folder",
        }
    }

    /// Is this data type cacheable? (i.e. can it be stored in the cache)
    #[must_use]
    pub fn is_cacheable(self) -> bool {
        match self {
            Self::Int | Self::String | Self::Container | Self::Service => true,
            Self::FileSystem => false,
        }
    }
}

/// A push-only store of T, returning stable IDs.
pub struct Store<T> {
    /// The backing storage of the items
    items: Vec<T>,
}

/// Id into a store of T.
///
/// This is generic over the type T to prevent mixing ids from different stores.
/// Although in theory a program can have multiple stores of the same type T, in which case
/// we would need to be careful to not mix the ids.
/// in practice serpentine only has one store per type T.
pub struct StoreId<T> {
    /// The index into the store
    index: usize,
    /// Phantom data to tie this id to the type T.
    /// Uses `fn() -> T` so that `StoreId` is unconditionally `Send + Sync`
    /// (it is just an index and never actually owns a `T`).
    _marker: std::marker::PhantomData<fn() -> T>,
}

#[cfg(test)]
impl<T: 'static> bolero::TypeGenerator for StoreId<T> {
    fn generate<D: bolero::Driver>(driver: &mut D) -> Option<Self> {
        Some(StoreId {
            index: usize::generate(driver)?,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T> StoreId<T> {
    /// Return the index of this id
    ///
    /// This should only be used for secondary maps.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }
}

impl<T> Clone for StoreId<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for StoreId<T> {}
impl<T> std::fmt::Debug for StoreId<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "StoreId({})", self.index)
    }
}

impl<T> PartialEq for StoreId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<T> Eq for StoreId<T> {}
impl<T> Hash for StoreId<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T> Store<T> {
    /// Create a new empty store
    #[must_use]
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Push a new item to the store, returning its id
    #[must_use = "The id is the only way to refer to the item."]
    pub fn push(&mut self, item: T) -> StoreId<T> {
        let id = StoreId {
            index: self.items.len(),
            _marker: std::marker::PhantomData,
        };
        self.items.push(item);
        id
    }

    /// Get an item from its id.
    ///
    /// This will panic if a id from a different store is used.
    /// In general no stores over the same T should be active in the program at the same time to
    /// make this case impossible.
    #[expect(clippy::expect_used, reason = "Store ids are always valid")]
    #[must_use]
    pub fn get(&self, id: StoreId<T>) -> &T {
        self.items
            .get(id.index)
            .expect("Store id out of bounds of store.")
    }

    /// Get the length of the store
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

impl<T> IntoIterator for Store<T> {
    type IntoIter = std::vec::IntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

/// Id for referencing the node implementation
pub type NodeKindId = StoreId<Box<dyn NodeImpl>>;

/// Stores the node implementations
pub type NodeStorage = Store<Box<dyn NodeImpl>>;

/// A node in the graph
#[derive(Hash, PartialEq, Eq, Clone)]
pub struct Node {
    /// The kind of this node
    pub kind: NodeKindId,
    /// The node ids for this inputs
    pub inputs: Box<[NodeInstanceId]>,
    /// Phantom inputs to this node, these will be resolved before the nodes actual logic runs.
    pub phantom_inputs: Box<[NodeInstanceId]>,
}

/// Id for referencing a node in the graph
pub type NodeInstanceId = StoreId<Spanned<Node>>;

/// Contains the graph
pub type Graph = Store<Spanned<Node>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cacheable_checks_agree() {
        bolero::check!().with_type().for_each(|data: &Data| {
            assert_eq!(data.cacheable().is_some(), data.type_().is_cacheable());
        });
    }
}
