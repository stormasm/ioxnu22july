//! This module handles the manipulation / execution of storage
//! plans. This is currently implemented using DataFusion, and this
//! interface abstracts away many of the details
pub(crate) mod context;
pub mod field;
pub mod fieldlist;
mod non_null_checker;
mod query_tracing;
mod schema_pivot;
pub mod seriesset;
pub(crate) mod split;
pub mod stringset;
pub use context::{DEFAULT_CATALOG, DEFAULT_SCHEMA};
use executor::DedicatedExecutor;
use trace::span::{SpanExt, SpanRecorder};

use std::sync::Arc;

use datafusion::{
    self,
    execution::{
        context::SessionState,
        runtime_env::{RuntimeConfig, RuntimeEnv},
    },
    logical_plan::{normalize_col, plan::Extension, Expr, LogicalPlan},
    prelude::SessionContext,
};

pub use context::{IOxSessionConfig, IOxSessionContext, SessionContextIOxExt};
use schema_pivot::SchemaPivotNode;

use self::{non_null_checker::NonNullCheckerNode, split::StreamSplitNode};

/// Configuration for an Executor
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Number of threads per thread pool
    pub num_threads: usize,

    /// Target parallelism for query execution
    pub target_query_partitions: usize,
}

/// Handles executing DataFusion plans, and marshalling the results into rust
/// native structures.
///
/// TODO: Have a resource manager that would limit how many plans are
/// running, based on a policy
#[derive(Debug)]
pub struct Executor {
    /// Executor for running user queries
    query_exec: DedicatedExecutor,

    /// Executor for running system/reorganization tasks such as
    /// compact
    reorg_exec: DedicatedExecutor,

    /// The default configuration options with which to create contexts
    config: ExecutorConfig,

    /// The DataFusion [RuntimeEnv] (including memory manager and disk
    /// manager) used for all executions
    runtime: Arc<RuntimeEnv>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorType {
    /// Run using the pool for queries
    Query,
    /// Run using the pool for system / reorganization tasks
    Reorg,
}

impl Executor {
    /// Creates a new executor with a two dedicated thread pools, each
    /// with num_threads
    pub fn new(num_threads: usize) -> Self {
        Self::new_with_config(ExecutorConfig {
            num_threads,
            target_query_partitions: num_threads,
        })
    }

    pub fn new_with_config(config: ExecutorConfig) -> Self {
        let query_exec = DedicatedExecutor::new("IOx Query Executor Thread", config.num_threads);
        let reorg_exec = DedicatedExecutor::new("IOx Reorg Executor Thread", config.num_threads);

        let runtime_config = RuntimeConfig::new();
        let runtime = Arc::new(RuntimeEnv::new(runtime_config).expect("creating runtime"));

        Self {
            query_exec,
            reorg_exec,
            config,
            runtime,
        }
    }

    /// Return a new execution config, suitable for executing a new query or system task.
    ///
    /// Note that this context (and all its clones) will be shut down once `Executor` is dropped.
    pub fn new_execution_config(&self, executor_type: ExecutorType) -> IOxSessionConfig {
        let exec = self.executor(executor_type).clone();
        IOxSessionConfig::new(exec, Arc::clone(&self.runtime))
            .with_target_partitions(self.config.target_query_partitions)
    }

    /// Get IOx context from DataFusion state.
    pub fn new_context_from_df(
        &self,
        executor_type: ExecutorType,
        state: &SessionState,
    ) -> IOxSessionContext {
        let inner = SessionContext::with_state(state.clone());
        let exec = self.executor(executor_type).clone();
        let recorder = SpanRecorder::new(state.span_ctx().child_span("Query Execution"));
        IOxSessionContext::new(inner, Some(exec), recorder)
    }

    /// Create a new execution context, suitable for executing a new query or system task
    ///
    /// Note that this context (and all its clones) will be shut down once `Executor` is dropped.
    pub fn new_context(&self, executor_type: ExecutorType) -> IOxSessionContext {
        self.new_execution_config(executor_type).build()
    }

    /// Return the execution pool  of the specified type
    fn executor(&self, executor_type: ExecutorType) -> &DedicatedExecutor {
        match executor_type {
            ExecutorType::Query => &self.query_exec,
            ExecutorType::Reorg => &self.reorg_exec,
        }
    }

    /// Initializes shutdown.
    pub fn shutdown(&self) {
        self.query_exec.shutdown();
        self.reorg_exec.shutdown();
    }

    /// Stops all subsequent task executions, and waits for the worker
    /// thread to complete. Note this will shutdown all created contexts.
    ///
    /// Only the first all to `join` will actually wait for the
    /// executing thread to complete. All other calls to join will
    /// complete immediately.
    pub async fn join(&self) {
        self.query_exec.join().await;
        self.reorg_exec.join().await;
    }
}

// No need to implement `Drop` because this is done by DedicatedExecutor already

/// Create a SchemaPivot node which  an arbitrary input like
///  ColA | ColB | ColC
/// ------+------+------
///   1   | NULL | NULL
///   2   | 2    | NULL
///   3   | 2    | NULL
///
/// And pivots it to a table with a single string column for any
/// columns that had non null values.
///
///   non_null_column
///  -----------------
///   "ColA"
///   "ColB"
pub fn make_schema_pivot(input: LogicalPlan) -> LogicalPlan {
    let node = Arc::new(SchemaPivotNode::new(input));

    LogicalPlan::Extension(Extension { node })
}

/// Make a NonNullChecker node takes an arbitrary input array and
/// produces a single string output column that contains
///
/// 1. the single `table_name` string if any of the input columns are non-null
/// 2. zero rows if all of the input columns are null
///
/// For this input:
///
///  ColA | ColB | ColC
/// ------+------+------
///   1   | NULL | NULL
///   2   | 2    | NULL
///   3   | 2    | NULL
///
/// The output would be (given 'the_table_name' was the table name)
///
///   non_null_column
///  -----------------
///   the_table_name
///
/// However, for this input (All NULL)
///
///  ColA | ColB | ColC
/// ------+------+------
///  NULL | NULL | NULL
///  NULL | NULL | NULL
///  NULL | NULL | NULL
///
/// There would be no output rows
///
///   non_null_column
///  -----------------
pub fn make_non_null_checker(table_name: &str, input: LogicalPlan) -> LogicalPlan {
    let node = Arc::new(NonNullCheckerNode::new(table_name, input));

    LogicalPlan::Extension(Extension { node })
}

/// Create a StreamSplit node which takes an input stream of record
/// batches and produces multiple output streams based on  a list of `N` predicates.
/// The output will have `N+1` streams, and each row is sent to the stream
/// corresponding to the first predicate that evaluates to true, or the last stream if none do.
///
/// For example, if the input looks like:
/// ```text
///  X | time
/// ---+-----
///  a | 1000
///  b | 4000
///  c | 2000
/// ```
///
/// A StreamSplit with split_exprs = [`time <= 1000`, `1000 < time <=2000`] will produce the
/// following three output streams (output DataFusion Partitions):
///
///
/// ```text
///  X | time
/// ---+-----
///  a | 1000
/// ```
///
/// ```text
///  X | time
/// ---+-----
///  b | 2000
/// ```
/// and
/// ```text
///  X | time
/// ---+-----
///  b | 4000
/// ```
pub fn make_stream_split(input: LogicalPlan, split_exprs: Vec<Expr>) -> LogicalPlan {
    // rewrite the input expression so that it is fully qualified with the input schema
    let split_exprs = split_exprs
        .into_iter()
        .map(|split_expr| normalize_col(split_expr, &input).expect("normalize is infallable"))
        .collect::<Vec<_>>();

    let node = Arc::new(StreamSplitNode::new(input, split_exprs));
    LogicalPlan::Extension(Extension { node })
}

/// A type that can provide `IOxSessionContext` for query
pub trait ExecutionContextProvider {
    /// Returns a new execution context suitable for running queries
    fn new_query_context(&self, span_ctx: Option<trace::ctx::SpanContext>) -> IOxSessionContext;
}

#[cfg(test)]
mod tests {
    use arrow::{
        array::{ArrayRef, Int64Array, StringArray},
        datatypes::{DataType, Field, Schema, SchemaRef},
    };
    use datafusion::{
        datasource::MemTable,
        logical_plan::{provider_as_source, LogicalPlanBuilder},
    };
    use stringset::StringSet;

    use super::*;
    use crate::exec::stringset::StringSetRef;
    use crate::plan::stringset::StringSetPlan;
    use arrow::record_batch::RecordBatch;

    #[tokio::test]
    async fn executor_known_string_set_plan_ok() {
        let expected_strings = to_set(&["Foo", "Bar"]);
        let plan = StringSetPlan::Known(Arc::clone(&expected_strings));

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let result_strings = ctx.to_string_set(plan).await.unwrap();
        assert_eq!(result_strings, expected_strings);

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_single_plan_no_batches() {
        // Test with a single plan that produces no batches
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Utf8, true)]));
        let scan = make_plan(schema, vec![]);
        let plan: StringSetPlan = vec![scan].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await.unwrap();

        assert_eq!(results, StringSetRef::new(StringSet::new()));

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_single_plan_one_batch() {
        // Test with a single plan that produces one record batch
        let data = to_string_array(&["foo", "bar", "baz", "foo"]);
        let batch = RecordBatch::try_from_iter_with_nullable(vec![("a", data, true)])
            .expect("created new record batch");
        let scan = make_plan(batch.schema(), vec![batch]);
        let plan: StringSetPlan = vec![scan].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await.unwrap();

        assert_eq!(results, to_set(&["foo", "bar", "baz"]));

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_single_plan_two_batch() {
        // Test with a single plan that produces multiple record batches
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Utf8, true)]));
        let data1 = to_string_array(&["foo", "bar"]);
        let batch1 = RecordBatch::try_new(Arc::clone(&schema), vec![data1])
            .expect("created new record batch");
        let data2 = to_string_array(&["baz", "foo"]);
        let batch2 = RecordBatch::try_new(Arc::clone(&schema), vec![data2])
            .expect("created new record batch");
        let scan = make_plan(schema, vec![batch1, batch2]);
        let plan: StringSetPlan = vec![scan].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await.unwrap();

        assert_eq!(results, to_set(&["foo", "bar", "baz"]));

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_multi_plan() {
        // Test with multiple datafusion logical plans
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Utf8, true)]));

        let data1 = to_string_array(&["foo", "bar"]);
        let batch1 = RecordBatch::try_new(Arc::clone(&schema), vec![data1])
            .expect("created new record batch");
        let scan1 = make_plan(Arc::clone(&schema), vec![batch1]);

        let data2 = to_string_array(&["baz", "foo"]);
        let batch2 = RecordBatch::try_new(Arc::clone(&schema), vec![data2])
            .expect("created new record batch");
        let scan2 = make_plan(schema, vec![batch2]);

        let plan: StringSetPlan = vec![scan1, scan2].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await.unwrap();

        assert_eq!(results, to_set(&["foo", "bar", "baz"]));

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_nulls() {
        // Ensure that nulls in the output set are handled reasonably
        // (error, rather than silently ignored)
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Utf8, true)]));
        let array = StringArray::from_iter(vec![Some("foo"), None]);
        let data = Arc::new(array);
        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![data])
            .expect("created new record batch");
        let scan = make_plan(schema, vec![batch]);
        let plan: StringSetPlan = vec![scan].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await;

        let actual_error = match results {
            Ok(_) => "Unexpected Ok".into(),
            Err(e) => format!("{}", e),
        };
        let expected_error = "unexpected null value";
        assert!(
            actual_error.contains(expected_error),
            "expected error '{}' not found in '{:?}'",
            expected_error,
            actual_error,
        );

        exec.join().await;
    }

    #[tokio::test]
    async fn executor_datafusion_string_set_bad_schema() {
        // Ensure that an incorect schema (an int) gives a reasonable error
        let data: ArrayRef = Arc::new(Int64Array::from(vec![1]));
        let batch =
            RecordBatch::try_from_iter(vec![("a", data)]).expect("created new record batch");
        let scan = make_plan(batch.schema(), vec![batch]);
        let plan: StringSetPlan = vec![scan].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await;

        let actual_error = match results {
            Ok(_) => "Unexpected Ok".into(),
            Err(e) => format!("{}", e),
        };

        let expected_error = "schema not a single Utf8";
        assert!(
            actual_error.contains(expected_error),
            "expected error '{}' not found in '{:?}'",
            expected_error,
            actual_error
        );

        exec.join().await;
    }

    #[tokio::test]
    async fn make_schema_pivot_is_planned() {
        // Test that all the planning logic is wired up and that we
        // can make a plan using a SchemaPivot node
        let batch = RecordBatch::try_from_iter_with_nullable(vec![
            ("f1", to_string_array(&["foo", "bar"]), true),
            ("f2", to_string_array(&["baz", "bzz"]), true),
        ])
        .expect("created new record batch");

        let scan = make_plan(batch.schema(), vec![batch]);
        let pivot = make_schema_pivot(scan);
        let plan = vec![pivot].into();

        let exec = Executor::new(1);
        let ctx = exec.new_context(ExecutorType::Query);
        let results = ctx.to_string_set(plan).await.expect("Executed plan");

        assert_eq!(results, to_set(&["f1", "f2"]));

        exec.join().await;
    }

    /// return a set for testing
    fn to_set(strs: &[&str]) -> StringSetRef {
        StringSetRef::new(strs.iter().map(|s| s.to_string()).collect::<StringSet>())
    }

    fn to_string_array(strs: &[&str]) -> ArrayRef {
        let array: StringArray = strs.iter().map(|s| Some(*s)).collect();
        Arc::new(array)
    }

    // creates a DataFusion plan that reads the RecordBatches into memory
    fn make_plan(schema: SchemaRef, data: Vec<RecordBatch>) -> LogicalPlan {
        let partitions = vec![data];

        let projection = None;

        // model one partition,
        let table = MemTable::try_new(schema, partitions).unwrap();
        let source = provider_as_source(Arc::new(table));

        LogicalPlanBuilder::scan("memtable", source, projection)
            .unwrap()
            .build()
            .unwrap()
    }
}
