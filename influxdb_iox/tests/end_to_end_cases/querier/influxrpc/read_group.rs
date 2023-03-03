use super::{dump::dump_data_frames, read_group_data};
use futures::{prelude::*, FutureExt};
use generated_types::storage_client::StorageClient;
use influxdb_iox_client::connection::Connection;
use test_helpers_end_to_end::{
    maybe_skip_integration, GrpcRequestBuilder, MiniCluster, Step, StepTest, StepTestState,
};

use generated_types::{aggregate::AggregateType, read_group_request::Group, ReadGroupRequest};

/// Standalone test for read_group with group keys and no aggregate
#[tokio::test]
async fn test_read_group_none_agg() {
    do_read_group_test(
        read_group_data(),
        GrpcRequestBuilder::new()
            .timestamp_range(0, 2001) // include all data
            .group_keys(["cpu"])
            .group(Group::By)
            .aggregate_type(AggregateType::None),
        vec![
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu1",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"20,21\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"10,11\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"81,82\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"71,72\"",
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu2",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"40,41\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"30,31\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"51,52\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000, 2000], values: \"61,62\"",
        ],
    )
    .await
}

/// Test that predicates make it through
#[tokio::test]
async fn test_read_group_none_agg_with_predicate() {
    do_read_group_test(
        read_group_data(),
        GrpcRequestBuilder::new()
            // do not include data at timestamp 2000
            .timestamp_range(0, 2000)
            .field_predicate("usage_system")
            .group_keys(["cpu"])
            .group(Group::By)
            .aggregate_type(AggregateType::None),
        vec![
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu1",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000], values: \"20\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000], values: \"10\"",
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu2",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [1000], values: \"40\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [1000], values: \"30\"",
        ],
    )
    .await
}

// Standalone test for read_group with group keys and an actual
// "aggregate" (not a "selector" style).  assumes that
// read_group_data has been previously loaded
#[tokio::test]
async fn test_read_group_sum_agg() {
    do_read_group_test(
        read_group_data(),
        // read_group(group_keys: region, agg: Sum)
        GrpcRequestBuilder::new()
            .timestamp_range(0, 2001) // include all data
            .group_keys(["cpu"])
            .group(Group::By)
            .aggregate_type(AggregateType::Sum),
        vec![
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu1",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"41\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"21\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"163\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"143\"",
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu2",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"81\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"61\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"103\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"123\"",
        ],
    )
    .await;
}

// Standalone test for read_group with group keys the count aggregate
// (returns a different type than the field types)
#[tokio::test]
async fn test_read_group_count_agg() {
    do_read_group_test(
        read_group_data(),
        // read_group(group_keys: region, agg: Count)
        GrpcRequestBuilder::new()
            .timestamp_range(0, 2001) // include all data
            .group_keys(["cpu"])
            .group(Group::By)
            .aggregate_type(AggregateType::Count),
        vec![
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu1",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=bar, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=foo, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=bar, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=foo, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu2",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=bar, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=foo, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=bar, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=foo, type: 1",
            "IntegerPointsFrame, timestamps: [2000], values: \"2\"",
        ],
    )
    .await
}

// Standalone test for read_group with group keys and an actual
// "selector" function last.  assumes that
// read_group_data has been previously loaded
#[tokio::test]
async fn test_read_group_last_agg() {
    do_read_group_test(
        read_group_data(),
        // read_group(group_keys: region, agg: Last)
        GrpcRequestBuilder::new()
            .timestamp_range(0, 2001) // include all data
            .group_keys(["cpu"])
            .group(Group::By)
            .aggregate_type(AggregateType::Last),
        vec![
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu1",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"21\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"11\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"82\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu1,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"72\"",
            "GroupFrame, tag_keys: _field,_measurement,cpu,host, partition_key_vals: cpu2",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"41\"",
            "SeriesFrame, tags: _field=usage_system,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"31\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=bar, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"52\"",
            "SeriesFrame, tags: _field=usage_user,_measurement=cpu,cpu=cpu2,host=foo, type: 0",
            "FloatPointsFrame, timestamps: [2000], values: \"62\"",
        ],
    )
    .await
}

/// Sends the specified line protocol to a server, runs a read_grou
/// gRPC request, and compares it against expected frames
async fn do_read_group_test(
    input_lines: Vec<&str>,
    request_builder: GrpcRequestBuilder,
    expected_frames: impl IntoIterator<Item = &str>,
) {
    test_helpers::maybe_start_logging();
    let database_url = maybe_skip_integration!();
    let expected_frames: Vec<String> = expected_frames.into_iter().map(|s| s.to_string()).collect();

    // Set up the cluster  ====================================
    let mut cluster = MiniCluster::create_shared(database_url).await;

    let line_protocol = input_lines.join("\n");
    StepTest::new(
        &mut cluster,
        vec![
            Step::WriteLineProtocol(line_protocol),
            Step::WaitForReadable,
            Step::Custom(Box::new(move |state: &mut StepTestState| {
                async move {
                    let mut storage_client =
                        StorageClient::new(state.cluster().querier().querier_grpc_connection());

                    println!("Sending read_group request with {:#?}", request_builder);

                    let read_group_request =
                        request_builder.source(state.cluster()).build_read_group();

                    assert_eq!(
                        do_read_group_request(&mut storage_client, read_group_request).await,
                        expected_frames,
                    );
                }
                .boxed()
            })),
        ],
    )
    .run()
    .await
}

/// Make a read_group request and returns the results in a comparable format
async fn do_read_group_request(
    storage_client: &mut StorageClient<Connection>,
    request: tonic::Request<ReadGroupRequest>,
) -> Vec<String> {
    let read_group_response = storage_client
        .read_group(request)
        .await
        .expect("successful read_group call");

    let responses: Vec<_> = read_group_response
        .into_inner()
        .try_collect()
        .await
        .unwrap();

    let frames: Vec<_> = responses
        .into_iter()
        .flat_map(|r| r.frames)
        .flat_map(|f| f.data)
        .collect();

    dump_data_frames(&frames)
}
