//! Implementation of command line option for running the compactor

use iox_query::exec::Executor;
use iox_time::{SystemProvider, TimeProvider};
use object_store::DynObjectStore;
use object_store_metrics::ObjectStoreMetrics;
use observability_deps::tracing::*;
use std::sync::Arc;
use thiserror::Error;

use clap_blocks::object_store::make_object_store;
use clap_blocks::{
    catalog_dsn::CatalogDsnConfig, compactor::CompactorConfig, run_config::RunConfig,
};
use ioxd_common::server_type::{CommonServerState, CommonServerStateError};
use ioxd_common::Service;
use ioxd_compactor::create_compactor_server_type;

use super::main;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Run: {0}")]
    Run(#[from] main::Error),

    #[error("Invalid config: {0}")]
    InvalidConfig(#[from] CommonServerStateError),

    #[error("Catalog error: {0}")]
    Catalog(#[from] iox_catalog::interface::Error),

    #[error("Catalog DSN error: {0}")]
    CatalogDsn(#[from] clap_blocks::catalog_dsn::Error),

    #[error("Cannot parse object store config: {0}")]
    ObjectStoreParsing(#[from] clap_blocks::object_store::ParseError),

    #[error("error initializing compactor: {0}")]
    Compactor(#[from] ioxd_compactor::Error),
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "run",
    about = "Runs in compactor mode",
    long_about = "Run the IOx compactor server.\n\nThe configuration options below can be \
    set either with the command line flags or with the specified environment \
    variable. If there is a file named '.env' in the current working directory, \
    it is sourced before loading the configuration.

Configuration is loaded from the following sources (highest precedence first):
        - command line arguments
        - user set environment variables
        - .env file contents
        - pre-configured default values"
)]
pub struct Config {
    #[clap(flatten)]
    pub(crate) run_config: RunConfig,

    #[clap(flatten)]
    pub(crate) catalog_dsn: CatalogDsnConfig,

    #[clap(flatten)]
    pub(crate) compactor_config: CompactorConfig,

    /// Number of threads to use for the compactor query execution, compaction and persistence.
    #[clap(
        long = "--query-exec-thread-count",
        env = "INFLUXDB_IOX_QUERY_EXEC_THREAD_COUNT",
        default_value = "4",
        action
    )]
    pub query_exec_thread_count: usize,
}

pub async fn command(config: Config) -> Result<(), Error> {
    let common_state = CommonServerState::from_config(config.run_config.clone())?;

    let time_provider = Arc::new(SystemProvider::new()) as Arc<dyn TimeProvider>;
    let metric_registry: Arc<metric::Registry> = Default::default();
    let catalog = config
        .catalog_dsn
        .get_catalog("compactor", Arc::clone(&metric_registry))
        .await?;

    let object_store = make_object_store(config.run_config.object_store_config())
        .map_err(Error::ObjectStoreParsing)?;

    // Decorate the object store with a metric recorder.
    let object_store: Arc<DynObjectStore> = Arc::new(ObjectStoreMetrics::new(
        object_store,
        Arc::clone(&time_provider),
        &*metric_registry,
    ));

    let exec = Arc::new(Executor::new(config.query_exec_thread_count));
    let time_provider = Arc::new(SystemProvider::new());

    let server_type = create_compactor_server_type(
        &common_state,
        Arc::clone(&metric_registry),
        catalog,
        object_store,
        exec,
        time_provider,
        config.compactor_config,
    )
    .await?;

    info!("starting compactor");

    let services = vec![Service::create(server_type, common_state.run_config())];
    Ok(main::main(common_state, services, metric_registry).await?)
}
