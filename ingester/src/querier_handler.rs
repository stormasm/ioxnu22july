//! Handle all requests from Querier

use crate::data::{
    IngesterData, IngesterQueryPartition, IngesterQueryResponse, QueryableBatch,
    UnpersistedPartitionData,
};
use arrow::error::ArrowError;
use datafusion::{
    error::DataFusionError, logical_plan::LogicalPlanBuilder,
    physical_plan::SendableRecordBatchStream,
};
use futures::StreamExt;
use generated_types::ingester::IngesterQueryRequest;
use iox_query::{
    exec::{Executor, ExecutorType},
    QueryChunk, QueryChunkMeta, ScanPlanBuilder,
};
use observability_deps::tracing::debug;
use predicate::Predicate;
use schema::selection::Selection;
use snafu::{ensure, ResultExt, Snafu};
use std::sync::Arc;

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub enum Error {
    #[snafu(display("Error creating plan for querying Ingester data to send to Querier"))]
    FrontendError {
        source: iox_query::frontend::common::Error,
    },

    #[snafu(display("Error building logical plan for querying Ingester data to send to Querier"))]
    LogicalPlan { source: DataFusionError },

    #[snafu(display(
        "Error building physical plan for querying Ingester data to send to Querier: {}",
        source
    ))]
    PhysicalPlan { source: DataFusionError },

    #[snafu(display(
        "Error executing the query for getting Ingester data to send to Querier: {}",
        source
    ))]
    ExecutePlan { source: DataFusionError },

    #[snafu(display("Error collecting a stream of record batches: {}", source))]
    CollectStream { source: DataFusionError },

    #[snafu(display(
        "No Namespace Data found for the given namespace name {}",
        namespace_name,
    ))]
    NamespaceNotFound { namespace_name: String },

    #[snafu(display(
        "No Table Data found for the given namespace name {}, table name {}",
        namespace_name,
        table_name
    ))]
    TableNotFound {
        namespace_name: String,
        table_name: String,
    },

    #[snafu(display("Error concating same-schema record batches: {}", source))]
    ConcatBatches { source: arrow::error::ArrowError },

    #[snafu(display(
        "Cannot apply identical schema to record batches of all partitions: {}",
        source
    ))]
    InterPartitionSchemaApplication { source: arrow::error::ArrowError },

    #[snafu(display("Concurrent query request limit exceeded"))]
    RequestLimit,
}

/// A specialized `Error` for Ingester's Query errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Return data to send as a response back to the Querier per its request
pub async fn prepare_data_to_querier(
    ingest_data: &Arc<IngesterData>,
    request: &Arc<IngesterQueryRequest>,
) -> Result<IngesterQueryResponse> {
    debug!(?request, "prepare_data_to_querier");
    let mut unpersisted_partitions = vec![];
    let mut found_namespace = false;
    for (sequencer_id, sequencer_data) in ingest_data.sequencers() {
        debug!(sequencer_id=%sequencer_id.get());
        let namespace_data = match sequencer_data.namespace(&request.namespace) {
            Some(namespace_data) => {
                debug!(namespace=%request.namespace, "found namespace");
                found_namespace = true;
                namespace_data
            }
            None => {
                continue;
            }
        };

        let table_data = match namespace_data.table_data(&request.table) {
            Some(table_data) => {
                debug!(table_name=%request.table, "found table");
                table_data
            }
            None => {
                continue;
            }
        };

        let mut unpersisted_partition_data = {
            let table_data = table_data.read().await;
            table_data.unpersisted_partition_data()
        };
        debug!(?unpersisted_partition_data);

        unpersisted_partitions.append(&mut unpersisted_partition_data);
    }

    ensure!(
        found_namespace,
        NamespaceNotFoundSnafu {
            namespace_name: &request.namespace,
        },
    );
    ensure!(
        !unpersisted_partitions.is_empty(),
        TableNotFoundSnafu {
            namespace_name: &request.namespace,
            table_name: &request.table
        },
    );

    let ingest_data = Arc::clone(ingest_data);
    let request = Arc::clone(request);
    let partitions = futures::stream::iter(unpersisted_partitions).then(move |partition| {
        let ingest_data = Arc::clone(&ingest_data);
        let request = Arc::clone(&request);

        async move {
            // extract payload
            let partition_id = partition.partition_id;
            let status = partition.partition_status.clone();
            let snapshots: Vec<_> =
                prepare_data_to_querier_for_partition(ingest_data.exec(), partition, &request)
                    .await
                    .map_err(|e| ArrowError::ExternalError(Box::new(e)))?
                    .into_iter()
                    .map(Ok)
                    .collect();

            // Note: include partition in `unpersisted_partitions` even when there we might filter out all the data, because
            // the metadata (e.g. max persisted parquet file) is important for the querier.
            Ok(IngesterQueryPartition::new(
                Box::pin(futures::stream::iter(snapshots)),
                partition_id,
                status,
            ))
        }
    });

    Ok(IngesterQueryResponse::new(Box::pin(partitions)))
}

async fn prepare_data_to_querier_for_partition(
    executor: &Executor,
    unpersisted_partition_data: UnpersistedPartitionData,
    request: &IngesterQueryRequest,
) -> Result<Option<SendableRecordBatchStream>> {
    // ------------------------------------------------
    // Accumulate data

    // Make Filters
    let selection_columns: Vec<_> = request.columns.iter().map(String::as_str).collect();
    let selection = if selection_columns.is_empty() {
        Selection::All
    } else {
        Selection::Some(&selection_columns)
    };
    let predicate = request.predicate.clone().unwrap_or_default();

    // figure out what batches
    let queryable_batch = unpersisted_partition_data
        .persisting
        .unwrap_or_else(|| QueryableBatch::new(&request.table, vec![], vec![]))
        .with_data(unpersisted_partition_data.non_persisted);

    // No data!
    if queryable_batch.data.is_empty() {
        return Ok(None);
    }

    query(
        executor,
        Arc::new(queryable_batch),
        predicate.clone(),
        selection,
    )
    .await
    .map(Some)
}

/// Query a given Queryable Batch, applying selection and filters as appropriate
/// Return stream of record batches
pub(crate) async fn query(
    executor: &Executor,
    data: Arc<QueryableBatch>,
    predicate: Predicate,
    selection: Selection<'_>,
) -> Result<SendableRecordBatchStream> {
    // Build logical plan for filtering data
    // Note that this query will also apply the delete predicates that go with the QueryableBatch

    let mut expr = vec![];
    if let Some(filter_expr) = predicate.filter_expr() {
        expr.push(filter_expr);
    }

    // TODO: Since we have different type of servers (router,
    // ingester, compactor, and querier), we may want to add more
    // types into the ExecutorType to have better log and resource
    // managment
    let ctx = executor.new_context(ExecutorType::Query);

    // Creates an execution plan for a scan and filter data of a single chunk
    let schema = data.schema();
    let table_name = data.table_name().to_string();

    debug!(%table_name, ?predicate, "Creating single chunk scan plan");

    let logical_plan = ScanPlanBuilder::new(schema, ctx.child_ctx("scan_and_filter planning"))
        .with_predicate(&predicate)
        .with_chunks([data as _])
        .build()
        .context(FrontendSnafu)?
        .plan_builder
        .build()
        .context(LogicalPlanSnafu)?;

    debug!(%table_name, plan=%logical_plan.display_indent_schema(),
           "created single chunk scan plan");

    // Now, restrict to all columns that are relevant
    let logical_plan = match selection {
        Selection::All => logical_plan,
        Selection::Some(cols) => {
            // filter out columns that are not in the schema
            let schema = Arc::clone(logical_plan.schema());
            let cols = cols.iter().filter_map(|col_name| {
                schema
                    .index_of_column_by_name(None, col_name)
                    .ok()
                    .map(|_| datafusion::prelude::col(col_name))
            });

            LogicalPlanBuilder::from(logical_plan)
                .project(cols)
                .context(LogicalPlanSnafu)?
                .build()
                .context(LogicalPlanSnafu)?
        }
    };

    // Build physical plan
    let physical_plan = ctx
        .create_physical_plan(&logical_plan)
        .await
        .context(PhysicalPlanSnafu {})?;

    // Execute the plan and return the filtered stream
    let output_stream = ctx
        .execute_stream(physical_plan)
        .await
        .context(ExecutePlanSnafu {})?;

    Ok(output_stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data::FlatIngesterQueryResponse,
        test_util::{
            create_one_record_batch_with_influxtype_no_duplicates, create_tombstone,
            make_ingester_data, make_ingester_data_with_tombstones, make_queryable_batch,
            make_queryable_batch_with_deletes, DataLocation, TEST_NAMESPACE, TEST_TABLE,
        },
    };
    use arrow::record_batch::RecordBatch;
    use arrow_util::{assert_batches_eq, assert_batches_sorted_eq};
    use assert_matches::assert_matches;
    use datafusion::logical_plan::{col, lit};
    use futures::TryStreamExt;
    use predicate::Predicate;

    #[tokio::test]
    async fn test_query() {
        test_helpers::maybe_start_logging();

        // create input data
        let batches = create_one_record_batch_with_influxtype_no_duplicates().await;

        // build queryable batch from the input batches
        let batch = make_queryable_batch("test_table", 1, batches);

        // query without filters
        let exc = Executor::new(1);
        let stream = query(&exc, batch, Predicate::default(), Selection::All)
            .await
            .unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify data: all rows and columns should be returned
        let expected = vec![
            "+-----------+------+-----------------------------+",
            "| field_int | tag1 | time                        |",
            "+-----------+------+-----------------------------+",
            "| 70        | UT   | 1970-01-01T00:00:00.000020Z |",
            "| 10        | VT   | 1970-01-01T00:00:00.000010Z |",
            "| 1000      | WA   | 1970-01-01T00:00:00.000008Z |",
            "+-----------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);

        exc.join().await;
    }

    #[tokio::test]
    async fn test_query_with_filter() {
        test_helpers::maybe_start_logging();

        // create input data
        let batches = create_one_record_batch_with_influxtype_no_duplicates().await;

        // build queryable batch from the input batches
        let batch = make_queryable_batch("test_table", 1, batches);

        // make filters
        // Only read 2 columns: "tag1" and "time"
        let selection = Selection::Some(&["tag1", "time"]);

        // tag1=VT
        let expr = col("tag1").eq(lit("VT"));
        let pred = Predicate::default().with_expr(expr);

        let exc = Executor::new(1);
        let stream = query(&exc, batch, pred, selection).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify data: 2  columns and one row of "tag1=VT" should be returned
        let expected = vec![
            "+------+-----------------------------+",
            "| tag1 | time                        |",
            "+------+-----------------------------+",
            "| VT   | 1970-01-01T00:00:00.000010Z |",
            "+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);

        exc.join().await;
    }

    #[tokio::test]
    async fn test_query_with_filter_with_delete() {
        test_helpers::maybe_start_logging();

        // create input data
        let batches = create_one_record_batch_with_influxtype_no_duplicates().await;
        let tombstones = vec![create_tombstone(1, 1, 1, 1, 0, 200000, "tag1=UT")];

        // build queryable batch from the input batches
        let batch = make_queryable_batch_with_deletes("test_table", 1, batches, tombstones);

        // make filters
        // Only read 2 columns: "tag1" and "time"
        let selection = Selection::Some(&["tag1", "time"]);

        // tag1=UT
        let expr = col("tag1").eq(lit("UT"));
        let pred = Predicate::default().with_expr(expr);

        let exc = Executor::new(1);
        let stream = query(&exc, batch, pred, selection).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify data: return nothing because the selected row already deleted
        let expected = vec!["++", "++"];
        assert_batches_eq!(&expected, &output_batches);

        exc.join().await;
    }

    #[tokio::test]
    async fn test_prepare_data_to_querier() {
        test_helpers::maybe_start_logging();

        // make 14 scenarios for ingester data
        let mut scenarios = vec![];
        for two_partitions in [false, true] {
            for loc in [
                DataLocation::BUFFER,
                DataLocation::BUFFER_SNAPSHOT,
                DataLocation::BUFFER_PERSISTING,
                DataLocation::BUFFER_SNAPSHOT_PERSISTING,
                DataLocation::SNAPSHOT,
                DataLocation::SNAPSHOT_PERSISTING,
                DataLocation::PERSISTING,
            ] {
                let scenario = Arc::new(make_ingester_data(two_partitions, loc));
                scenarios.push((loc, scenario));
            }
        }

        // read data from all scenarios without any filters
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec![],
            None,
        ));
        let expected = vec![
            "+------------+-----+------+--------------------------------+",
            "| city       | day | temp | time                           |",
            "+------------+-----+------+--------------------------------+",
            "| Andover    | tue | 56   | 1970-01-01T00:00:00.000000030Z |", // in group 1 - seq_num: 2
            "| Andover    | mon |      | 1970-01-01T00:00:00.000000046Z |", // in group 2 - seq_num: 3
            "| Boston     | sun | 60   | 1970-01-01T00:00:00.000000036Z |", // in group 1 - seq_num: 1
            "| Boston     | mon |      | 1970-01-01T00:00:00.000000038Z |", // in group 3 - seq_num: 5
            "| Medford    | sun | 55   | 1970-01-01T00:00:00.000000022Z |", // in group 4 - seq_num: 7
            "| Medford    | wed |      | 1970-01-01T00:00:00.000000026Z |", // in group 2 - seq_num: 4
            "| Reading    | mon | 58   | 1970-01-01T00:00:00.000000040Z |", // in group 4 - seq_num: 8
            "| Wilmington | mon |      | 1970-01-01T00:00:00.000000035Z |", // in group 3 - seq_num: 6
            "+------------+-----+------+--------------------------------+",
        ];
        for (loc, scenario) in &scenarios {
            println!("Location: {loc:?}");
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }

        // read data from all scenarios and filter out column day
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec!["city".to_string(), "temp".to_string(), "time".to_string()],
            None,
        ));
        let expected = vec![
            "+------------+------+--------------------------------+",
            "| city       | temp | time                           |",
            "+------------+------+--------------------------------+",
            "| Andover    |      | 1970-01-01T00:00:00.000000046Z |",
            "| Andover    | 56   | 1970-01-01T00:00:00.000000030Z |",
            "| Boston     |      | 1970-01-01T00:00:00.000000038Z |",
            "| Boston     | 60   | 1970-01-01T00:00:00.000000036Z |",
            "| Medford    |      | 1970-01-01T00:00:00.000000026Z |",
            "| Medford    | 55   | 1970-01-01T00:00:00.000000022Z |",
            "| Reading    | 58   | 1970-01-01T00:00:00.000000040Z |",
            "| Wilmington |      | 1970-01-01T00:00:00.000000035Z |",
            "+------------+------+--------------------------------+",
        ];
        for (loc, scenario) in &scenarios {
            println!("Location: {loc:?}");
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }

        // read data from all scenarios, filter out column day, city Medford, time outside range [0, 42)
        let expr = col("city").not_eq(lit("Medford"));
        let pred = Predicate::default().with_expr(expr).with_range(0, 42);
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec!["city".to_string(), "temp".to_string(), "time".to_string()],
            Some(pred),
        ));
        let expected = vec![
            "+------------+------+--------------------------------+",
            "| city       | temp | time                           |",
            "+------------+------+--------------------------------+",
            "| Andover    | 56   | 1970-01-01T00:00:00.000000030Z |",
            "| Boston     |      | 1970-01-01T00:00:00.000000038Z |",
            "| Boston     | 60   | 1970-01-01T00:00:00.000000036Z |",
            "| Reading    | 58   | 1970-01-01T00:00:00.000000040Z |",
            "| Wilmington |      | 1970-01-01T00:00:00.000000035Z |",
            "+------------+------+--------------------------------+",
        ];
        for (loc, scenario) in &scenarios {
            println!("Location: {loc:?}");
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }

        // test "table not found" handling
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            "table_does_not_exist".to_string(),
            vec![],
            None,
        ));
        for (loc, scenario) in &scenarios {
            println!("Location: {loc:?}");
            let err = prepare_data_to_querier(scenario, &request)
                .await
                .unwrap_err();
            assert_matches!(err, Error::TableNotFound { .. });
        }

        // test "namespace not found" handling
        let request = Arc::new(IngesterQueryRequest::new(
            "namespace_does_not_exist".to_string(),
            TEST_TABLE.to_string(),
            vec![],
            None,
        ));
        for (loc, scenario) in &scenarios {
            println!("Location: {loc:?}");
            let err = prepare_data_to_querier(scenario, &request)
                .await
                .unwrap_err();
            assert_matches!(err, Error::NamespaceNotFound { .. });
        }
    }

    #[tokio::test]
    async fn test_prepare_data_to_querier_with_tombstones() {
        test_helpers::maybe_start_logging();

        // make 7 scenarios for ingester data with tombstones
        let mut scenarios = vec![];
        for loc in &[
            DataLocation::BUFFER,
            DataLocation::BUFFER_SNAPSHOT,
            DataLocation::BUFFER_PERSISTING,
            DataLocation::BUFFER_SNAPSHOT_PERSISTING,
            DataLocation::SNAPSHOT,
            DataLocation::SNAPSHOT_PERSISTING,
            DataLocation::PERSISTING,
        ] {
            let scenario = Arc::new(make_ingester_data_with_tombstones(*loc).await);
            scenarios.push(scenario);
        }

        // read data from all scenarios without any filters
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec![],
            None,
        ));
        let expected = vec![
            "+------------+-----+------+--------------------------------+",
            "| city       | day | temp | time                           |",
            "+------------+-----+------+--------------------------------+",
            "| Andover    | mon |      | 1970-01-01T00:00:00.000000046Z |",
            "| Andover    | tue | 56   | 1970-01-01T00:00:00.000000030Z |",
            "| Medford    | sun | 55   | 1970-01-01T00:00:00.000000022Z |",
            "| Medford    | wed |      | 1970-01-01T00:00:00.000000026Z |",
            "| Reading    | mon | 58   | 1970-01-01T00:00:00.000000040Z |",
            "| Wilmington | mon |      | 1970-01-01T00:00:00.000000035Z |",
            "+------------+-----+------+--------------------------------+",
        ];
        for scenario in &scenarios {
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }

        // read data from all scenarios and filter out column day
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec!["city".to_string(), "temp".to_string(), "time".to_string()],
            None,
        ));
        let expected = vec![
            "+------------+------+--------------------------------+",
            "| city       | temp | time                           |",
            "+------------+------+--------------------------------+",
            "| Andover    |      | 1970-01-01T00:00:00.000000046Z |",
            "| Andover    | 56   | 1970-01-01T00:00:00.000000030Z |",
            "| Medford    |      | 1970-01-01T00:00:00.000000026Z |",
            "| Medford    | 55   | 1970-01-01T00:00:00.000000022Z |",
            "| Reading    | 58   | 1970-01-01T00:00:00.000000040Z |",
            "| Wilmington |      | 1970-01-01T00:00:00.000000035Z |",
            "+------------+------+--------------------------------+",
        ];
        for scenario in &scenarios {
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }

        // read data from all scenarios, filter out column day, city Medford, time outside range [0, 42)
        let expr = col("city").not_eq(lit("Medford"));
        let pred = Predicate::default().with_expr(expr).with_range(0, 42);
        let request = Arc::new(IngesterQueryRequest::new(
            TEST_NAMESPACE.to_string(),
            TEST_TABLE.to_string(),
            vec!["city".to_string(), "temp".to_string(), "time".to_string()],
            Some(pred),
        ));
        let expected = vec![
            "+------------+------+--------------------------------+",
            "| city       | temp | time                           |",
            "+------------+------+--------------------------------+",
            "| Andover    | 56   | 1970-01-01T00:00:00.000000030Z |",
            "| Reading    | 58   | 1970-01-01T00:00:00.000000040Z |",
            "| Wilmington |      | 1970-01-01T00:00:00.000000035Z |",
            "+------------+------+--------------------------------+",
        ];
        for scenario in &scenarios {
            let stream = prepare_data_to_querier(scenario, &request).await.unwrap();
            let result = ingester_response_to_record_batches(stream).await;
            assert_batches_sorted_eq!(&expected, &result);
        }
    }

    async fn ingester_response_to_record_batches(
        response: IngesterQueryResponse,
    ) -> Vec<RecordBatch> {
        let mut last_schema = None;
        let mut batches = vec![];

        let mut stream = response.flatten();
        while let Some(msg) = stream.try_next().await.unwrap() {
            match msg {
                FlatIngesterQueryResponse::StartPartition { .. } => (),
                FlatIngesterQueryResponse::RecordBatch { batch } => {
                    let last_schema = last_schema.as_ref().unwrap();
                    assert_eq!(&batch.schema(), last_schema);
                    batches.push(batch);
                }
                FlatIngesterQueryResponse::StartSnapshot { schema } => {
                    last_schema = Some(schema);
                }
            }
        }

        batches
    }
}
