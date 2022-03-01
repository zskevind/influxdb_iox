use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use backoff::{Backoff, BackoffConfig};
use iox_catalog::interface::{Catalog, NamespaceId};
use object_store::ObjectStore;
use observability_deps::tracing::info;
use parking_lot::RwLock;
use time::TimeProvider;
use tokio_util::sync::CancellationToken;

use crate::{cache::CatalogCache, namespace::QuerierNamespace};

const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Database for the querier.
///
/// Contains all namespaces.
#[derive(Debug)]
pub struct QuerierDatabase {
    /// Backoff config for IO operations.
    backoff_config: BackoffConfig,

    /// Catalog.
    catalog: Arc<dyn Catalog>,

    /// Catalog cache.
    catalog_cache: Arc<CatalogCache>,

    /// Metric registry
    metric_registry: Arc<metric::Registry>,

    /// Namespaces.
    namespaces: RwLock<HashMap<Arc<str>, Arc<QuerierNamespace>>>,

    /// Object store.
    object_store: Arc<ObjectStore>,

    /// Time provider.
    time_provider: Arc<dyn TimeProvider>,
}

impl QuerierDatabase {
    /// Create new, empty database.
    ///
    /// You may call [`sync`](Self::sync) to fill this database with content.
    pub fn new(
        catalog: Arc<dyn Catalog>,
        metric_registry: Arc<metric::Registry>,
        object_store: Arc<ObjectStore>,
        time_provider: Arc<dyn TimeProvider>,
    ) -> Self {
        let catalog_cache = Arc::new(CatalogCache::new(Arc::clone(&catalog)));

        Self {
            backoff_config: BackoffConfig::default(),
            catalog,
            catalog_cache,
            metric_registry,
            namespaces: RwLock::new(HashMap::new()),
            object_store,
            time_provider,
        }
    }

    /// List of namespaces.
    pub fn namespaces(&self) -> Vec<Arc<QuerierNamespace>> {
        self.namespaces.read().values().cloned().collect()
    }

    /// Sync set of namespaces and the data of the namespaces themselves.
    ///
    /// Should be called regularly.
    pub async fn sync(&self) {
        let namespaces = Backoff::new(&self.backoff_config)
            .retry_all_errors("get namespaces", || async {
                self.catalog.repositories().await.namespaces().list().await
            })
            .await
            .expect("unlimited retry");
        let namespaces: BTreeMap<Arc<str>, NamespaceId> = namespaces
            .into_iter()
            .map(|ns| (ns.name.into(), ns.id))
            .collect();

        // lock namespaces AFTER IO
        let querier_namespaces: Vec<_> = {
            let mut namespaces_guard = self.namespaces.write();

            // calculate set differences
            let to_add: Vec<(Arc<str>, NamespaceId)> = namespaces
                .iter()
                .filter_map(|(name, id)| {
                    (!namespaces_guard.contains_key(name)).then(|| (Arc::clone(name), *id))
                })
                .collect();
            let to_delete: Vec<Arc<str>> = namespaces_guard
                .keys()
                .filter_map(|name| (!namespaces.contains_key(name)).then(|| Arc::clone(name)))
                .collect();
            info!(
                add = to_add.len(),
                delete = to_delete.len(),
                "Syncing namespaces",
            );

            // perform modification
            for name in to_delete {
                // TODO(marco): this is currently untested because `iox_catalog` doesn't implement namespace deletion
                namespaces_guard.remove(&name);
            }
            for (name, id) in to_add {
                namespaces_guard.insert(
                    Arc::clone(&name),
                    Arc::new(QuerierNamespace::new(
                        Arc::clone(&self.catalog_cache),
                        name,
                        id,
                        Arc::clone(&self.metric_registry),
                        Arc::clone(&self.object_store),
                        Arc::clone(&self.time_provider),
                    )),
                );
            }

            // get a clone of namespace Arcs so that we can run an async operation
            namespaces_guard.values().cloned().collect()
        };

        // downgrade guard so that other readers are allowed again and sync namespace states.
        for namespace in querier_namespaces {
            namespace.sync().await;
        }
    }
}

/// Run regular [`sync`](QuerierDatabase::sync) until shutdown token is canceled.
pub(crate) async fn database_sync_loop(
    database: Arc<QuerierDatabase>,
    shutdown: CancellationToken,
) {
    loop {
        if shutdown.is_cancelled() {
            info!("Database sync shutdown");
            return;
        }

        database.sync().await;

        tokio::select!(
            _ = tokio::time::sleep(SYNC_INTERVAL) => {},
            _ = shutdown.cancelled() => {},
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::TestCatalog;

    use super::*;

    #[tokio::test]
    async fn test_sync() {
        let catalog = TestCatalog::new();

        let db = QuerierDatabase::new(
            catalog.catalog(),
            catalog.metric_registry(),
            catalog.object_store(),
            catalog.time_provider(),
        );
        assert_eq!(ns_names(&db), vec![]);

        db.sync().await;
        assert_eq!(ns_names(&db), vec![]);

        catalog.create_namespace("ns1").await;
        catalog.create_namespace("ns2").await;
        db.sync().await;
        assert_eq!(ns_names(&db), vec![Arc::from("ns1"), Arc::from("ns2")]);

        catalog.create_namespace("ns3").await;
        db.sync().await;
        assert_eq!(
            ns_names(&db),
            vec![Arc::from("ns1"), Arc::from("ns2"), Arc::from("ns3")]
        );
    }

    fn ns_names(db: &QuerierDatabase) -> Vec<Arc<str>> {
        let mut names: Vec<Arc<str>> = db.namespaces().iter().map(|ns| ns.name()).collect();
        names.sort();
        names
    }
}
