//! Implementation of command line option for running server

use std::sync::Arc;

use clap_blocks::run_config::RunConfig;
use ioxd_common::server_type::{CommonServerState, CommonServerStateError};
use ioxd_common::Service;
use ioxd_test::{TestAction, TestServerType};
use metric::Registry;
use thiserror::Error;

use super::main;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Run: {0}")]
    Run(#[from] main::Error),

    #[error("Invalid config: {0}")]
    InvalidConfig(#[from] CommonServerStateError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, clap::Parser)]
#[clap(
    name = "run",
    about = "Runs in test mode",
    long_about = "Run the IOx test server.\n\nThe configuration options below can be \
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

    /// Test action
    #[clap(
        arg_enum,
        long = "--test-action",
        env = "IOX_TEST_ACTION",
        default_value = "None",
        ignore_case = true,
        action
    )]
    test_action: TestAction,
}

pub async fn command(config: Config) -> Result<()> {
    let common_state = CommonServerState::from_config(config.run_config.clone())?;
    let server_type = Arc::new(TestServerType::new(
        Arc::new(Registry::new()),
        common_state.trace_collector(),
        config.test_action,
    ));

    let services = vec![Service::create(server_type, common_state.run_config())];
    Ok(main::main(
        common_state,
        services,
        Arc::new(metric::Registry::default()),
    )
    .await?)
}
