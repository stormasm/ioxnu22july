//! Caching of [`NamespaceSchema`].

mod memory;
pub use memory::*;

mod sharded_cache;
pub use sharded_cache::*;

pub mod metrics;

use data_types::{DatabaseName, NamespaceSchema};
use std::{fmt::Debug, sync::Arc};

/// An abstract cache of [`NamespaceSchema`].
pub trait NamespaceCache: Debug + Send + Sync {
    /// Return the [`NamespaceSchema`] for `namespace`.
    fn get_schema(&self, namespace: &DatabaseName<'_>) -> Option<Arc<NamespaceSchema>>;

    /// Place `schema` in the cache, unconditionally overwriting any existing
    /// [`NamespaceSchema`] mapped to `namespace`, returning
    /// the previous value, if any
    fn put_schema(
        &self,
        namespace: DatabaseName<'static>,
        schema: impl Into<Arc<NamespaceSchema>>,
    ) -> Option<Arc<NamespaceSchema>>;
}
