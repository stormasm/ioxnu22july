use std::{sync::Arc, time::Instant};

use arrow::{
    array::{ArrayRef, Int64Array, StringArray},
    record_batch::RecordBatch,
};
use observability_deps::tracing::{debug, info};
use snafu::{ResultExt, Snafu};

use influxdb_iox_client::{
    connection::Connection, flight::generated_types::ReadInfo, format::QueryOutputFormat,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Error loading remote state: {}", source))]
    LoadingRemoteState {
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    #[snafu(display("Error formatting results: {}", source))]
    FormattingResults {
        source: influxdb_iox_client::format::Error,
    },

    #[snafu(display("Error setting format to '{}': {}", requested_format, source))]
    SettingFormat {
        requested_format: String,
        source: influxdb_iox_client::format::Error,
    },

    #[snafu(display("Error running remote query: {}", source))]
    RunningRemoteQuery {
        source: influxdb_iox_client::flight::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum QueryEngine {
    /// Run queries against the named database on the remote server
    Remote(String),
}

#[derive(Debug)]
pub struct Nuclient {
    /// Client for interacting with IOx namespace API
    namespace_client: influxdb_iox_client::namespace::Client,

    /// Client for running sql
    flight_client: influxdb_iox_client::flight::Client,

    /// database name against which SQL commands are run
    query_engine: Option<QueryEngine>,

    /// Formatter to use to format query results
    output_format: QueryOutputFormat,
}

impl Nuclient {
    /// Create a new Nuclient instance, connected to the specified URL
    pub fn new(connection: Connection) -> Self {
        let namespace_client = influxdb_iox_client::namespace::Client::new(connection.clone());
        let flight_client = influxdb_iox_client::flight::Client::new(connection.clone());

        let output_format = QueryOutputFormat::Pretty;

        Self {
            namespace_client,
            flight_client,
            query_engine: None,
            output_format,
        }
    }

    // get all namespaces in csv output
    pub async fn list_namespaces(&mut self) -> Result<String> {
        let namespaces = self
            .namespace_client
            .get_namespaces()
            .await
            .map_err(|e| Box::new(e) as _)
            .context(LoadingRemoteStateSnafu)?;

        let namespace_id: Int64Array = namespaces.iter().map(|ns| Some(ns.id)).collect();
        let name: StringArray = namespaces.iter().map(|ns| Some(&ns.name)).collect();

        let record_batch = RecordBatch::try_from_iter(vec![
            ("namespace_id", Arc::new(namespace_id) as ArrayRef),
            ("name", Arc::new(name) as ArrayRef),
        ])
        .expect("creating record batch successfully");

        let result_str = self.get_results(&[record_batch])?;
        Ok(result_str)
    }

    // Run a command against the currently selected remote database
    pub async fn run_sql(&mut self, sql: String) -> Result<String> {
        let batches = match &mut self.query_engine {
            None => {
                println!("Error: no database selected.");
                println!("Hint: Run USE DATABASE <dbname> to select database");
                return Ok("Error: no database selected".to_string());
            }
            Some(QueryEngine::Remote(db_name)) => {
                info!(%db_name, %sql, "Running sql on remote database");

                scrape_query(&mut self.flight_client, db_name, &sql).await?
            }
        };

        let result_str = self.get_results(&batches)?;

        Ok(result_str)
    }

    // Run a command against the currently selected remote database
    pub async fn print_sql(&mut self, sql: String) -> Result<()> {
        let start = Instant::now();

        let batches = match &mut self.query_engine {
            None => {
                println!("Error: no database selected.");
                println!("Hint: Run USE DATABASE <dbname> to select database");
                return Ok(());
            }
            Some(QueryEngine::Remote(db_name)) => {
                info!(%db_name, %sql, "Running sql on remote database");

                scrape_query(&mut self.flight_client, db_name, &sql).await?
            }
        };

        let end = Instant::now();
        self.print_results(&batches)?;

        println!(
            "Returned {} in {:?}",
            Self::row_summary(&batches),
            end - start
        );
        Ok(())
    }

    fn row_summary<'a>(batches: impl IntoIterator<Item = &'a RecordBatch>) -> String {
        let total_rows: usize = batches.into_iter().map(|b| b.num_rows()).sum();

        if total_rows > 1 {
            format!("{} rows", total_rows)
        } else if total_rows == 0 {
            "no rows".to_string()
        } else {
            "1 row".to_string()
        }
    }

    pub fn use_database(&mut self, db_name: String) {
        debug!(%db_name, "setting current database");
        println!("You are now querying the database {}", db_name);
        self.set_query_engine(QueryEngine::Remote(db_name));
    }

    pub fn set_query_engine(&mut self, query_engine: QueryEngine) {
        self.query_engine = Some(query_engine)
    }

    /// Sets the output format to the specified format
    pub fn set_output_format<S: AsRef<str>>(&mut self, requested_format: S) -> Result<()> {
        let requested_format = requested_format.as_ref();

        self.output_format = requested_format
            .parse()
            .context(SettingFormatSnafu { requested_format })?;
        // leave this here for future debugging...
        // println!("Set output format to {}", self.output_format);
        Ok(())
    }

    /// Prints to the specified output format
    fn get_results(&self, batches: &[RecordBatch]) -> Result<String> {
        let formatted_results = self
            .output_format
            .format(batches)
            .context(FormattingResultsSnafu)?;
        //println!("{}", formatted_results);
        Ok(formatted_results)
    }

    /// Prints to the specified output format
    fn print_results(&self, batches: &[RecordBatch]) -> Result<()> {
        let formatted_results = self
            .output_format
            .format(batches)
            .context(FormattingResultsSnafu)?;
        println!("{}", formatted_results);
        Ok(())
    }
}

/// Runs the specified `query` and returns the record batches of the result
async fn scrape_query(
    client: &mut influxdb_iox_client::flight::Client,
    db_name: &str,
    query: &str,
) -> Result<Vec<RecordBatch>> {
    let mut query_results = client
        .perform_query(ReadInfo {
            namespace_name: db_name.to_string(),
            sql_query: query.to_string(),
        })
        .await
        .context(RunningRemoteQuerySnafu)?;

    let mut batches = vec![];

    while let Some(data) = query_results
        .next()
        .await
        .context(RunningRemoteQuerySnafu)?
    {
        batches.push(data);
    }

    Ok(batches)
}
