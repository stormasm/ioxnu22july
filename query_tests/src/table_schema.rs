//! Tests for the table_names implementation

use arrow::datatypes::DataType;
use iox_query::QueryChunk;
use schema::selection::Selection;
use schema::{builder::SchemaBuilder, sort::SortKey, Schema, TIME_COLUMN_NAME};

use super::scenarios::*;

/// Creates and loads several database scenarios using the db_setup
/// function.
///
/// runs table_schema(predicate) and compares it to the expected
/// output
async fn run_table_schema_test_case<D>(
    db_setup: D,
    selection: Selection<'_>,
    table_name: &str,
    expected_schema: Schema,
    expected_sort_key: Option<&SortKey>,
) where
    D: DbSetup,
{
    test_helpers::maybe_start_logging();

    for scenario in db_setup.make().await {
        let DbScenario {
            scenario_name, db, ..
        } = scenario;
        println!("Running scenario '{}'", scenario_name);
        println!(
            "Getting schema for table '{}', selection {:?}",
            table_name, selection
        );

        // Make sure at least one table has data
        let mut chunks_with_table = 0;

        let ctx = db.new_query_context(None);
        let chunks = db
            .chunks(table_name, &Default::default(), ctx)
            .await
            .expect("error getting chunks");
        for chunk in chunks {
            if chunk.table_name() == table_name {
                chunks_with_table += 1;
                let actual_schema = chunk.schema().select(selection).unwrap();

                assert_eq!(
                    expected_schema,
                    actual_schema,
                    "Mismatch in chunk {}\nExpected:\n{:#?}\nActual:\n{:#?}\n",
                    chunk.id(),
                    expected_schema,
                    actual_schema
                );

                // There are a few cases where we don't care about the sort key:
                // - no "expected" value was provided which is interpreted as "don't care"; some
                //   chunk representations are always sorted
                // - the chunk is in some known-to-be-always-unsorted stage
                if expected_sort_key.is_some() && !is_unsorted_chunk_type(chunk.as_ref()) {
                    assert_eq!(chunk.sort_key(), expected_sort_key);
                }
            }
        }
        assert!(
            chunks_with_table > 0,
            "Expected at least one chunk to have data, but none did"
        );
    }
}

fn is_unsorted_chunk_type(chunk: &dyn QueryChunk) -> bool {
    chunk.chunk_type() == "IngesterPartition"
}

#[tokio::test]
async fn list_schema_cpu_all() {
    // we expect columns to come out in lexicographic order by name
    // The schema of RUB includes sort key
    let sort_key = SortKey::from_columns(vec!["region", TIME_COLUMN_NAME]);

    let expected_schema = SchemaBuilder::new()
        .tag("region")
        .timestamp()
        .field("user", DataType::Float64)
        .build()
        .unwrap();

    run_table_schema_test_case(
        TwoMeasurements {},
        Selection::All,
        "cpu",
        expected_schema,
        Some(&sort_key),
    )
    .await;
}

#[tokio::test]
async fn list_schema_cpu_all_set_sort_key() {
    // The schema of RUB includes sort key
    let sort_key = SortKey::from_columns(vec!["region", TIME_COLUMN_NAME]);

    let expected_schema = SchemaBuilder::new()
        .tag("region")
        .timestamp()
        .field("user", DataType::Float64)
        .build()
        .unwrap();

    run_table_schema_test_case(
        TwoMeasurements {},
        Selection::All,
        "cpu",
        expected_schema,
        Some(&sort_key),
    )
    .await;

    // Now set
}

#[tokio::test]
async fn list_schema_disk_all() {
    // we expect columns to come out in lexicographic order by name
    let expected_schema = SchemaBuilder::new()
        .field("bytes", DataType::Int64)
        .tag("region")
        .timestamp()
        .build()
        .unwrap();

    run_table_schema_test_case(
        TwoMeasurements {},
        Selection::All,
        "disk",
        expected_schema,
        None,
    )
    .await;
}

#[tokio::test]
async fn list_schema_cpu_selection() {
    let expected_schema = SchemaBuilder::new()
        .field("user", DataType::Float64)
        .tag("region")
        .build()
        .unwrap();

    // Pick an order that is not lexographic
    let selection = Selection::Some(&["user", "region"]);

    run_table_schema_test_case(TwoMeasurements {}, selection, "cpu", expected_schema, None).await;
}

#[tokio::test]
async fn list_schema_disk_selection() {
    // we expect columns to come out in lexicographic order by name
    let expected_schema = SchemaBuilder::new()
        .timestamp()
        .field("bytes", DataType::Int64)
        .build()
        .unwrap();

    // Pick an order that is not lexicographic
    let selection = Selection::Some(&["time", "bytes"]);

    run_table_schema_test_case(TwoMeasurements {}, selection, "disk", expected_schema, None).await;
}

#[tokio::test]
async fn list_schema_location_all() {
    // we expect columns to come out in lexicographic order by name
    let expected_schema = SchemaBuilder::new()
        .field("count", DataType::UInt64)
        .timestamp()
        .tag("town")
        .build()
        .unwrap();

    run_table_schema_test_case(
        TwoMeasurementsUnsignedType {},
        Selection::All,
        "restaurant",
        expected_schema,
        None,
    )
    .await;
}
