use super::NamespaceCache;
use data_types::{DatabaseName, NamespaceSchema};
use hashbrown::HashMap;
use parking_lot::RwLock;
use std::sync::Arc;

/// An in-memory cache of [`NamespaceSchema`] backed by a hashmap protected with
/// a read-write mutex.
#[derive(Debug, Default)]
pub struct MemoryNamespaceCache {
    cache: RwLock<HashMap<DatabaseName<'static>, Arc<NamespaceSchema>>>,
}

impl NamespaceCache for Arc<MemoryNamespaceCache> {
    fn get_schema(&self, namespace: &DatabaseName<'_>) -> Option<Arc<NamespaceSchema>> {
        self.cache.read().get(namespace).map(Arc::clone)
    }

    fn put_schema(
        &self,
        namespace: DatabaseName<'static>,
        schema: impl Into<Arc<NamespaceSchema>>,
    ) -> Option<Arc<NamespaceSchema>> {
        self.cache.write().insert(namespace, schema.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_types::{KafkaTopicId, NamespaceId, QueryPoolId};

    #[test]
    fn test_put_get() {
        let ns = DatabaseName::new("test").expect("database name is valid");
        let cache = Arc::new(MemoryNamespaceCache::default());

        assert!(cache.get_schema(&ns).is_none());

        let schema1 = NamespaceSchema {
            id: NamespaceId::new(42),
            kafka_topic_id: KafkaTopicId::new(24),
            query_pool_id: QueryPoolId::new(1234),
            tables: Default::default(),
        };
        assert!(cache.put_schema(ns.clone(), schema1.clone()).is_none());
        assert_eq!(*cache.get_schema(&ns).expect("lookup failure"), schema1);

        let schema2 = NamespaceSchema {
            id: NamespaceId::new(2),
            kafka_topic_id: KafkaTopicId::new(2),
            query_pool_id: QueryPoolId::new(2),
            tables: Default::default(),
        };

        assert_eq!(
            *cache
                .put_schema(ns.clone(), schema2.clone())
                .expect("should have existing schema"),
            schema1
        );
        assert_eq!(*cache.get_schema(&ns).expect("lookup failure"), schema2);
    }
}
