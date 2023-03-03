use snafu::{ResultExt, Snafu};
use trogging::cli::LoggingConfig;

pub(crate) mod all_in_one;
mod compactor;
mod garbage_collector;
mod ingester;
mod main;
mod querier;
mod router;
mod test;

#[derive(Debug, Snafu)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[snafu(display("Error in compactor subcommand: {}", source))]
    CompactorError { source: compactor::Error },

    #[snafu(display("Error in garbage collector subcommand: {}", source))]
    GarbageCollectorError { source: garbage_collector::Error },

    #[snafu(display("Error in querier subcommand: {}", source))]
    QuerierError { source: querier::Error },

    #[snafu(display("Error in router subcommand: {}", source))]
    RouterError { source: router::Error },

    #[snafu(display("Error in ingester subcommand: {}", source))]
    IngesterError { source: ingester::Error },

    #[snafu(display("Error in all in one subcommand: {}", source))]
    AllInOneError { source: all_in_one::Error },

    #[snafu(display("Error in test subcommand: {}", source))]
    TestError { source: test::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, clap::Parser)]
pub struct Config {
    /// Supports having all-in-one be the default command.
    #[clap(flatten)]
    all_in_one_config: all_in_one::Config,

    #[clap(subcommand)]
    command: Option<Command>,
}

impl Config {
    pub fn logging_config(&self) -> &LoggingConfig {
        match &self.command {
            None => &self.all_in_one_config.logging_config,
            Some(Command::Compactor(config)) => config.run_config.logging_config(),
            Some(Command::GarbageCollector(config)) => config.run_config.logging_config(),
            Some(Command::Querier(config)) => config.run_config.logging_config(),
            Some(Command::Router(config)) => config.run_config.logging_config(),
            Some(Command::Ingester(config)) => config.run_config.logging_config(),
            Some(Command::AllInOne(config)) => &config.logging_config,
            Some(Command::Test(config)) => config.run_config.logging_config(),
        }
    }
}

#[derive(Debug, clap::Parser)]
enum Command {
    /// Run the server in compactor mode
    Compactor(compactor::Config),

    /// Run the server in querier mode
    Querier(querier::Config),

    /// Run the server in router mode
    Router(router::Config),

    /// Run the server in ingester mode
    Ingester(ingester::Config),

    /// Run the server in "all in one" mode (Default)
    AllInOne(all_in_one::Config),

    /// Run the server in test mode
    Test(test::Config),

    /// Run the server in garbage collecter mode
    GarbageCollector(garbage_collector::Config),
}

pub async fn command(config: Config) -> Result<()> {
    match config.command {
        None => all_in_one::command(config.all_in_one_config)
            .await
            .context(AllInOneSnafu),
        Some(Command::Compactor(config)) => {
            compactor::command(config).await.context(CompactorSnafu)
        }
        Some(Command::GarbageCollector(config)) => garbage_collector::command(config)
            .await
            .context(GarbageCollectorSnafu),
        Some(Command::Querier(config)) => querier::command(config).await.context(QuerierSnafu),
        Some(Command::Router(config)) => router::command(config).await.context(RouterSnafu),
        Some(Command::Ingester(config)) => ingester::command(config).await.context(IngesterSnafu),
        Some(Command::AllInOne(config)) => all_in_one::command(config).await.context(AllInOneSnafu),
        Some(Command::Test(config)) => test::command(config).await.context(TestSnafu),
    }
}
