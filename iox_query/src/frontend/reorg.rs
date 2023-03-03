//! planning for physical reorganization operations (e.g. COMPACT)

use std::sync::Arc;

use datafusion::logical_plan::{col, lit_timestamp_nano, LogicalPlan};
use observability_deps::tracing::debug;
use schema::{sort::SortKey, Schema, TIME_COLUMN_NAME};

use crate::{
    exec::{make_stream_split, IOxSessionContext},
    QueryChunk,
};
use snafu::{ResultExt, Snafu};

use super::common::ScanPlanBuilder;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Chunk schema not compatible for compact plan: {}", source))]
    ChunkSchemaNotCompatible { source: schema::merge::Error },

    #[snafu(display("Reorg planner got error building plan: {}", source))]
    BuildingPlan {
        source: datafusion::error::DataFusionError,
    },

    #[snafu(display("Reorg planner got error building scan: {}", source))]
    BuildingScan {
        source: crate::frontend::common::Error,
    },

    #[snafu(display(
        "Reorg planner got error adding creating scan for {}: {}",
        table_name,
        source
    ))]
    CreatingScan {
        table_name: String,
        source: super::common::Error,
    },
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<datafusion::error::DataFusionError> for Error {
    fn from(source: datafusion::error::DataFusionError) -> Self {
        Self::BuildingPlan { source }
    }
}

/// Planner for physically rearranging chunk data. This planner
/// creates COMPACT and SPLIT plans for use in the database lifecycle manager
#[derive(Debug)]
pub struct ReorgPlanner {
    ctx: IOxSessionContext,
}

impl ReorgPlanner {
    pub fn new(ctx: IOxSessionContext) -> Self {
        Self { ctx }
    }

    /// Creates an execution plan for the COMPACT operations which does the following:
    ///
    /// 1. Merges chunks together into a single stream
    /// 2. Deduplicates via PK as necessary
    /// 3. Sorts the result according to the requested key
    ///
    /// The plan looks like:
    ///
    /// (Sort on output_sort)
    ///   (Scan chunks) <-- any needed deduplication happens here
    pub fn compact_plan<I>(
        &self,
        schema: Arc<Schema>,
        chunks: I,
        sort_key: SortKey,
    ) -> Result<LogicalPlan>
    where
        I: IntoIterator<Item = Arc<dyn QueryChunk>>,
    {
        let scan_plan = ScanPlanBuilder::new(schema, self.ctx.child_ctx("compact_plan"))
            .with_chunks(chunks)
            .with_sort_key(sort_key)
            .build()
            .context(BuildingScanSnafu)?;

        let plan = scan_plan.plan_builder.build()?;

        debug!(table_name=scan_plan.provider.table_name(), plan=%plan.display_indent_schema(),
               "created compact plan for table");

        Ok(plan)
    }

    /// Creates an execution plan for the SPLIT operations which does the following:
    ///
    /// 1. Merges chunks together into a single stream
    /// 2. Deduplicates via PK as necessary
    /// 3. Sorts the result according to the requested key
    /// 4. Splits the stream on value of the `time` column: Those
    ///    rows that are on or before the time and those that are after
    ///
    /// The plan looks like:
    ///
    /// (Split on Time)
    ///   (Sort on output_sort)
    ///     (Scan chunks) <-- any needed deduplication happens here
    ///
    /// The output execution plan has n "output streams" (DataFusion partition):
    /// Stream 0: Rows that have `time` *on or before* the `split_times[0]`
    /// Stream i (0 < i < split_times's length): Rows that have  `time` in range `(split_times[i-1], split_times[i]]`
    /// Stream n (n = split_times.len()): Rows that have `time` *after* all the split_times and NULL rows
    ///
    /// For example, if the input looks like:
    /// ```text
    ///  X | time
    /// ---+-----
    ///  b | 2000
    ///  a | 1000
    ///  c | 4000
    ///  d | 2000
    ///  e | 3000
    /// ```
    /// A split plan with `sort=time` and `split_times=[2000, 3000]` will produce the following three output streams
    ///
    /// ```text
    ///  X | time
    /// ---+-----
    ///  a | 1000
    ///  b | 2000
    ///  d | 2000
    /// ```
    ///
    /// ```text
    ///  X | time
    /// ---+-----
    ///  e | 3000
    /// ```
    ///
    /// ```text
    ///  X | time
    /// ---+-----
    ///  c | 4000
    /// ```
    pub fn split_plan<I>(
        &self,
        schema: Arc<Schema>,
        chunks: I,
        sort_key: SortKey,
        split_times: Vec<i64>,
    ) -> Result<LogicalPlan>
    where
        I: IntoIterator<Item = Arc<dyn QueryChunk>>,
    {
        // split_times must have values
        if split_times.is_empty() {
            panic!("Split plan does not accept empty split_times");
        }

        let scan_plan = ScanPlanBuilder::new(schema, self.ctx.child_ctx("split_plan"))
            .with_chunks(chunks)
            .with_sort_key(sort_key)
            .build()
            .context(BuildingScanSnafu)?;

        let mut split_exprs = Vec::with_capacity(split_times.len());
        // time <= split_times[0]
        split_exprs.push(col(TIME_COLUMN_NAME).lt_eq(lit_timestamp_nano(split_times[0])));
        // split_times[i-1] , time <= split_time[i]
        for i in 1..split_times.len() {
            if split_times[i - 1] >= split_times[i] {
                panic!(
                    "split_times[{}]: {} must be smaller than split_times[{}]: {}",
                    i - 1,
                    split_times[i - 1],
                    i,
                    split_times[i]
                );
            }
            split_exprs.push(
                col(TIME_COLUMN_NAME)
                    .gt(lit_timestamp_nano(split_times[i - 1]))
                    .and(col(TIME_COLUMN_NAME).lt_eq(lit_timestamp_nano(split_times[i]))),
            );
        }

        let plan = scan_plan.plan_builder.build().context(BuildingPlanSnafu)?;
        let plan = make_stream_split(plan, split_exprs);

        debug!(table_name=scan_plan.provider.table_name(), plan=%plan.display_indent_schema(),
               "created split plan for table");

        Ok(plan)
    }
}

#[cfg(test)]
mod test {
    use arrow_util::assert_batches_eq;
    use datafusion_util::{test_collect, test_collect_partition};
    use schema::merge::SchemaMerger;
    use schema::sort::SortKeyBuilder;

    use crate::{
        exec::{Executor, ExecutorType},
        test::{raw_data, TestChunk},
    };

    use super::*;

    async fn get_test_chunks() -> (Arc<Schema>, Vec<Arc<dyn QueryChunk>>) {
        // Chunk 1 with 5 rows of data on 2 tags
        let chunk1 = Arc::new(
            TestChunk::new("t")
                .with_time_column_with_stats(Some(50), Some(7000))
                .with_tag_column_with_stats("tag1", Some("AL"), Some("MT"))
                .with_i64_field_column("field_int")
                .with_five_rows_of_data(),
        ) as Arc<dyn QueryChunk>;

        // Chunk 2 has an extra field, and only 4 fields
        let chunk2 = Arc::new(
            TestChunk::new("t")
                .with_time_column_with_stats(Some(28000), Some(220000))
                .with_tag_column_with_stats("tag1", Some("UT"), Some("WA"))
                .with_i64_field_column("field_int")
                .with_i64_field_column("field_int2")
                .with_may_contain_pk_duplicates(true)
                .with_four_rows_of_data(),
        ) as Arc<dyn QueryChunk>;

        let expected = vec![
            "+-----------+------+--------------------------------+",
            "| field_int | tag1 | time                           |",
            "+-----------+------+--------------------------------+",
            "| 1000      | MT   | 1970-01-01T00:00:00.000001Z    |",
            "| 10        | MT   | 1970-01-01T00:00:00.000007Z    |",
            "| 70        | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 100       | AL   | 1970-01-01T00:00:00.000000050Z |",
            "| 5         | MT   | 1970-01-01T00:00:00.000005Z    |",
            "+-----------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &raw_data(&[Arc::clone(&chunk1)]).await);

        let expected = vec![
            "+-----------+------------+------+-----------------------------+",
            "| field_int | field_int2 | tag1 | time                        |",
            "+-----------+------------+------+-----------------------------+",
            "| 1000      | 1000       | WA   | 1970-01-01T00:00:00.000028Z |",
            "| 10        | 10         | VT   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | 70         | UT   | 1970-01-01T00:00:00.000220Z |",
            "| 50        | 50         | VT   | 1970-01-01T00:00:00.000210Z |",
            "+-----------+------------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &raw_data(&[Arc::clone(&chunk2)]).await);

        let schema = SchemaMerger::new()
            .merge(&chunk1.schema())
            .unwrap()
            .merge(&chunk2.schema())
            .unwrap()
            .build();

        (Arc::new(schema), vec![chunk1, chunk2])
    }

    #[tokio::test]
    async fn test_compact_plan() {
        test_helpers::maybe_start_logging();

        let (schema, chunks) = get_test_chunks().await;

        let sort_key = SortKeyBuilder::with_capacity(2)
            .with_col_opts("tag1", true, true)
            .with_col_opts(TIME_COLUMN_NAME, false, false)
            .build();

        let compact_plan = ReorgPlanner::new(IOxSessionContext::with_testing())
            .compact_plan(schema, chunks, sort_key)
            .expect("created compact plan");

        let executor = Executor::new(1);
        let physical_plan = executor
            .new_context(ExecutorType::Reorg)
            .create_physical_plan(&compact_plan)
            .await
            .unwrap();
        assert_eq!(
            physical_plan.output_partitioning().partition_count(),
            1,
            "{:?}",
            physical_plan.output_partitioning()
        );

        let batches = test_collect(physical_plan).await;

        // sorted on state ASC and time
        let expected = vec![
            "+-----------+------------+------+--------------------------------+",
            "| field_int | field_int2 | tag1 | time                           |",
            "+-----------+------------+------+--------------------------------+",
            "| 1000      | 1000       | WA   | 1970-01-01T00:00:00.000028Z    |",
            "| 50        | 50         | VT   | 1970-01-01T00:00:00.000210Z    |",
            "| 70        | 70         | UT   | 1970-01-01T00:00:00.000220Z    |",
            "| 1000      |            | MT   | 1970-01-01T00:00:00.000001Z    |",
            "| 5         |            | MT   | 1970-01-01T00:00:00.000005Z    |",
            "| 10        |            | MT   | 1970-01-01T00:00:00.000007Z    |",
            "| 70        |            | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 100       |            | AL   | 1970-01-01T00:00:00.000000050Z |",
            "+-----------+------------+------+--------------------------------+",
        ];

        assert_batches_eq!(&expected, &batches);

        executor.join().await;
    }

    #[tokio::test]
    async fn test_split_plan() {
        test_helpers::maybe_start_logging();
        // validate that the plumbing is all hooked up. The logic of
        // the operator is tested in its own module.
        let (schema, chunks) = get_test_chunks().await;

        let sort_key = SortKeyBuilder::with_capacity(2)
            .with_col_opts("time", false, false)
            .with_col_opts("tag1", false, true)
            .build();

        // split on 1000 should have timestamps 1000, 5000, and 7000
        let split_plan = ReorgPlanner::new(IOxSessionContext::with_testing())
            .split_plan(schema, chunks, sort_key, vec![1000])
            .expect("created compact plan");

        let executor = Executor::new(1);
        let physical_plan = executor
            .new_context(ExecutorType::Reorg)
            .create_physical_plan(&split_plan)
            .await
            .unwrap();

        assert_eq!(
            physical_plan.output_partitioning().partition_count(),
            2,
            "{:?}",
            physical_plan.output_partitioning()
        );

        // verify that the stream was split
        let batches0 = test_collect_partition(Arc::clone(&physical_plan), 0).await;

        // Note sorted on time
        let expected = vec![
            "+-----------+------------+------+--------------------------------+",
            "| field_int | field_int2 | tag1 | time                           |",
            "+-----------+------------+------+--------------------------------+",
            "| 100       |            | AL   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        |            | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 1000      |            | MT   | 1970-01-01T00:00:00.000001Z    |",
            "+-----------+------------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &batches0);

        let batches1 = test_collect_partition(physical_plan, 1).await;

        // Sorted on time
        let expected = vec![
            "+-----------+------------+------+-----------------------------+",
            "| field_int | field_int2 | tag1 | time                        |",
            "+-----------+------------+------+-----------------------------+",
            "| 5         |            | MT   | 1970-01-01T00:00:00.000005Z |",
            "| 10        |            | MT   | 1970-01-01T00:00:00.000007Z |",
            "| 1000      | 1000       | WA   | 1970-01-01T00:00:00.000028Z |",
            "| 50        | 50         | VT   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | 70         | UT   | 1970-01-01T00:00:00.000220Z |",
            "+-----------+------------+------+-----------------------------+",
        ];

        assert_batches_eq!(&expected, &batches1);

        executor.join().await;
    }

    #[tokio::test]
    async fn test_split_plan_multi_exps() {
        test_helpers::maybe_start_logging();
        // validate that the plumbing is all hooked up. The logic of
        // the operator is tested in its own module.
        let (schema, chunks) = get_test_chunks().await;

        let sort_key = SortKeyBuilder::with_capacity(2)
            .with_col_opts("time", false, false)
            .with_col_opts("tag1", false, true)
            .build();

        // split on 1000 and 7000
        let split_plan = ReorgPlanner::new(IOxSessionContext::with_testing())
            .split_plan(schema, chunks, sort_key, vec![1000, 7000])
            .expect("created compact plan");

        let executor = Executor::new(1);
        let physical_plan = executor
            .new_context(ExecutorType::Reorg)
            .create_physical_plan(&split_plan)
            .await
            .unwrap();

        assert_eq!(
            physical_plan.output_partitioning().partition_count(),
            3,
            "{:?}",
            physical_plan.output_partitioning()
        );

        // Verify that the stream was split

        // Note sorted on time
        // Should include time <= 1000
        let batches0 = test_collect_partition(Arc::clone(&physical_plan), 0).await;
        let expected = vec![
            "+-----------+------------+------+--------------------------------+",
            "| field_int | field_int2 | tag1 | time                           |",
            "+-----------+------------+------+--------------------------------+",
            "| 100       |            | AL   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        |            | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 1000      |            | MT   | 1970-01-01T00:00:00.000001Z    |",
            "+-----------+------------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &batches0);

        // Sorted on time
        // Should include 1000 < time <= 7000
        let batches1 = test_collect_partition(Arc::clone(&physical_plan), 1).await;
        let expected = vec![
            "+-----------+------------+------+-----------------------------+",
            "| field_int | field_int2 | tag1 | time                        |",
            "+-----------+------------+------+-----------------------------+",
            "| 5         |            | MT   | 1970-01-01T00:00:00.000005Z |",
            "| 10        |            | MT   | 1970-01-01T00:00:00.000007Z |",
            "+-----------+------------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &batches1);

        // Sorted on time
        // Should include 7000 < time
        let batches2 = test_collect_partition(physical_plan, 2).await;
        let expected = vec![
            "+-----------+------------+------+-----------------------------+",
            "| field_int | field_int2 | tag1 | time                        |",
            "+-----------+------------+------+-----------------------------+",
            "| 1000      | 1000       | WA   | 1970-01-01T00:00:00.000028Z |",
            "| 50        | 50         | VT   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | 70         | UT   | 1970-01-01T00:00:00.000220Z |",
            "+-----------+------------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &batches2);

        executor.join().await;
    }

    #[tokio::test]
    #[should_panic(expected = "Split plan does not accept empty split_times")]
    async fn test_split_plan_panic_empty() {
        test_helpers::maybe_start_logging();
        // validate that the plumbing is all hooked up. The logic of
        // the operator is tested in its own module.
        let (schema, chunks) = get_test_chunks().await;

        let sort_key = SortKeyBuilder::with_capacity(2)
            .with_col_opts("time", false, false)
            .with_col_opts("tag1", false, true)
            .build();

        // split on 1000 and 7000
        let _split_plan = ReorgPlanner::new(IOxSessionContext::with_testing())
            .split_plan(schema, chunks, sort_key, vec![]) // reason of panic: empty split_times
            .expect("created compact plan");
    }

    #[tokio::test]
    #[should_panic(expected = "split_times[0]: 1000 must be smaller than split_times[1]: 500")]
    async fn test_split_plan_panic_times() {
        test_helpers::maybe_start_logging();
        // validate that the plumbing is all hooked up. The logic of
        // the operator is tested in its own module.
        let (schema, chunks) = get_test_chunks().await;

        let sort_key = SortKeyBuilder::with_capacity(2)
            .with_col_opts("time", false, false)
            .with_col_opts("tag1", false, true)
            .build();

        // split on 1000 and 7000
        let _split_plan = ReorgPlanner::new(IOxSessionContext::with_testing())
            .split_plan(schema, chunks, sort_key, vec![1000, 500]) // reason of panic: split_times not in ascending order
            .expect("created compact plan");
    }
}
