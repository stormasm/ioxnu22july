//! This module provides a reference implementation of
//! [`QueryDatabase`] for use in testing.
//!
//! AKA it is a Mock

use crate::{
    exec::{
        stringset::{StringSet, StringSetRef},
        ExecutionContextProvider, Executor, ExecutorType, IOxSessionContext,
    },
    Predicate, PredicateMatch, QueryChunk, QueryChunkError, QueryChunkMeta, QueryCompletedToken,
    QueryDatabase, QueryDatabaseError, QueryText,
};
use arrow::{
    array::{
        ArrayRef, DictionaryArray, Int64Array, StringArray, TimestampNanosecondArray, UInt64Array,
    },
    datatypes::{DataType, Int32Type, TimeUnit},
    record_batch::RecordBatch,
};
use async_trait::async_trait;
use data_types::{
    ChunkId, ChunkOrder, ColumnSummary, DeletePredicate, InfluxDbType, PartitionId, StatValues,
    Statistics, TableSummary, TimestampMinMax,
};
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion_util::stream_from_batches;
use futures::StreamExt;
use hashbrown::HashSet;
use observability_deps::tracing::debug;
use parking_lot::Mutex;
use predicate::rpc_predicate::QueryDatabaseMeta;
use schema::{
    builder::SchemaBuilder, merge::SchemaMerger, selection::Selection, sort::SortKey,
    InfluxColumnType, Schema,
};
use std::{any::Any, collections::BTreeMap, fmt, num::NonZeroU64, sync::Arc};
use trace::ctx::SpanContext;

#[derive(Debug)]
pub struct TestDatabase {
    executor: Arc<Executor>,
    /// Partitions which have been saved to this test database
    /// Key is partition name
    /// Value is map of chunk_id to chunk
    partitions: Mutex<BTreeMap<String, BTreeMap<ChunkId, Arc<TestChunk>>>>,

    /// `column_names` to return upon next request
    column_names: Arc<Mutex<Option<StringSetRef>>>,

    /// The predicate passed to the most recent call to `chunks()`
    chunks_predicate: Mutex<Predicate>,
}

impl TestDatabase {
    pub fn new(executor: Arc<Executor>) -> Self {
        Self {
            executor,
            partitions: Default::default(),
            column_names: Default::default(),
            chunks_predicate: Default::default(),
        }
    }

    /// Add a test chunk to the database
    pub fn add_chunk(&self, partition_key: &str, chunk: Arc<TestChunk>) -> &Self {
        let mut partitions = self.partitions.lock();
        let chunks = partitions
            .entry(partition_key.to_string())
            .or_insert_with(BTreeMap::new);
        chunks.insert(chunk.id(), chunk);
        self
    }

    /// Add a test chunk to the database
    pub fn with_chunk(self, partition_key: &str, chunk: Arc<TestChunk>) -> Self {
        self.add_chunk(partition_key, chunk);
        self
    }

    /// Get the specified chunk
    pub fn get_chunk(&self, partition_key: &str, id: ChunkId) -> Option<Arc<TestChunk>> {
        self.partitions
            .lock()
            .get(partition_key)
            .and_then(|p| p.get(&id).cloned())
    }

    /// Return the most recent predicate passed to get_chunks()
    pub fn get_chunks_predicate(&self) -> Predicate {
        self.chunks_predicate.lock().clone()
    }

    /// Set the list of column names that will be returned on a call to
    /// column_names
    pub fn set_column_names(&self, column_names: Vec<String>) {
        let column_names = column_names.into_iter().collect::<StringSet>();
        let column_names = Arc::new(column_names);

        *Arc::clone(&self.column_names).lock() = Some(column_names)
    }
}

#[async_trait]
impl QueryDatabase for TestDatabase {
    async fn chunks(
        &self,
        table_name: &str,
        predicate: &Predicate,
        _ctx: IOxSessionContext,
    ) -> Result<Vec<Arc<dyn QueryChunk>>, QueryDatabaseError> {
        // save last predicate
        *self.chunks_predicate.lock() = predicate.clone();

        let partitions = self.partitions.lock();
        Ok(partitions
            .values()
            .flat_map(|x| x.values())
            .filter(|x| x.table_name == table_name)
            .map(|x| Arc::clone(x) as _)
            .collect())
    }

    fn record_query(
        &self,
        _ctx: &IOxSessionContext,
        _query_type: &str,
        _query_text: QueryText,
    ) -> QueryCompletedToken {
        QueryCompletedToken::new(|_| {})
    }

    fn as_meta(&self) -> &dyn QueryDatabaseMeta {
        self
    }
}

impl QueryDatabaseMeta for TestDatabase {
    fn table_schema(&self, table_name: &str) -> Option<Arc<Schema>> {
        let mut merger = SchemaMerger::new();
        let mut found_one = false;

        let partitions = self.partitions.lock();
        for partition in partitions.values() {
            for chunk in partition.values() {
                if chunk.table_name() == table_name {
                    merger = merger.merge(&chunk.schema()).expect("consistent schemas");
                    found_one = true;
                }
            }
        }

        found_one.then(|| Arc::new(merger.build()))
    }

    fn table_names(&self) -> Vec<String> {
        let mut values = HashSet::new();
        let partitions = self.partitions.lock();
        for chunks in partitions.values() {
            for chunk in chunks.values() {
                values.get_or_insert_owned(&chunk.table_name);
            }
        }

        values.into_iter().collect()
    }
}

impl ExecutionContextProvider for TestDatabase {
    fn new_query_context(&self, span_ctx: Option<SpanContext>) -> IOxSessionContext {
        // Note: unlike Db this does not register a catalog provider
        self.executor
            .new_execution_config(ExecutorType::Query)
            .with_span_context(span_ctx)
            .build()
    }
}

#[derive(Debug)]
pub struct TestChunk {
    /// Table name
    table_name: String,

    /// Schema of the table
    schema: Arc<Schema>,

    /// Return value for summary()
    table_summary: TableSummary,

    id: ChunkId,

    partition_id: Option<PartitionId>,

    /// Set the flag if this chunk might contain duplicates
    may_contain_pk_duplicates: bool,

    /// A copy of the captured predicates passed
    predicates: Mutex<Vec<Predicate>>,

    /// RecordBatches that are returned on each request
    table_data: Vec<Arc<RecordBatch>>,

    /// A saved error that is returned instead of actual results
    saved_error: Option<String>,

    /// Return value for apply_predicate, if desired
    predicate_match: Option<PredicateMatch>,

    /// Copy of delete predicates passed
    delete_predicates: Vec<Arc<DeletePredicate>>,

    /// Order of this chunk relative to other overlapping chunks.
    order: ChunkOrder,

    /// The sort key of this chunk
    sort_key: Option<SortKey>,

    /// The partition sort key of this chunk
    partition_sort_key: Option<SortKey>,

    /// Time range of the data
    timestamp_min_max: Option<TimestampMinMax>,
}

/// Implements a method for adding a column with default stats
macro_rules! impl_with_column {
    ($NAME:ident, $DATA_TYPE:ident) => {
        pub fn $NAME(self, column_name: impl Into<String>) -> Self {
            let column_name = column_name.into();

            let new_column_schema = SchemaBuilder::new()
                .field(&column_name, DataType::$DATA_TYPE)
                .build()
                .unwrap();
            self.add_schema_to_table(new_column_schema, true, None)
        }
    };
}

/// Implements a method for adding a column without any stats
macro_rules! impl_with_column_no_stats {
    ($NAME:ident, $DATA_TYPE:ident) => {
        pub fn $NAME(self, column_name: impl Into<String>) -> Self {
            let column_name = column_name.into();

            let new_column_schema = SchemaBuilder::new()
                .field(&column_name, DataType::$DATA_TYPE)
                .build()
                .unwrap();

            self.add_schema_to_table(new_column_schema, false, None)
        }
    };
}

/// Implements a method for adding a column with stats that have the specified min and max
macro_rules! impl_with_column_with_stats {
    ($NAME:ident, $DATA_TYPE:ident, $RUST_TYPE:ty, $STAT_TYPE:ident) => {
        pub fn $NAME(
            self,
            column_name: impl Into<String>,
            min: Option<$RUST_TYPE>,
            max: Option<$RUST_TYPE>,
        ) -> Self {
            let column_name = column_name.into();

            let new_column_schema = SchemaBuilder::new()
                .field(&column_name, DataType::$DATA_TYPE)
                .build()
                .unwrap();

            let stats = Statistics::$STAT_TYPE(StatValues {
                min,
                max,
                ..Default::default()
            });

            self.add_schema_to_table(new_column_schema, true, Some(stats))
        }
    };
}

impl TestChunk {
    pub fn new(table_name: impl Into<String>) -> Self {
        let table_name = table_name.into();
        Self {
            table_name,
            schema: Arc::new(SchemaBuilder::new().build().unwrap()),
            table_summary: TableSummary::default(),
            id: ChunkId::new_test(0),
            may_contain_pk_duplicates: Default::default(),
            predicates: Default::default(),
            table_data: Default::default(),
            saved_error: Default::default(),
            predicate_match: Default::default(),
            delete_predicates: Default::default(),
            order: ChunkOrder::MIN,
            sort_key: None,
            partition_sort_key: None,
            timestamp_min_max: None,
            partition_id: None,
        }
    }

    pub fn with_id(mut self, id: u128) -> Self {
        self.id = ChunkId::new_test(id);
        self
    }

    pub fn with_partition_id(mut self, id: i64) -> Self {
        self.partition_id = Some(PartitionId::new(id));
        self
    }

    /// specify that any call should result in an error with the message
    /// specified
    pub fn with_error(mut self, error_message: impl Into<String>) -> Self {
        self.saved_error = Some(error_message.into());
        self
    }

    /// specify that any call to apply_predicate should return this value
    pub fn with_predicate_match(mut self, predicate_match: PredicateMatch) -> Self {
        self.predicate_match = Some(predicate_match);
        self
    }

    /// Checks the saved error, and returns it if any, otherwise returns OK
    fn check_error(&self) -> Result<(), QueryChunkError> {
        if let Some(message) = self.saved_error.as_ref() {
            Err(message.clone().into())
        } else {
            Ok(())
        }
    }

    /// Set the `may_contain_pk_duplicates` flag
    pub fn with_may_contain_pk_duplicates(mut self, v: bool) -> Self {
        self.may_contain_pk_duplicates = v;
        self
    }

    /// Register a tag column with the test chunk with default stats
    pub fn with_tag_column(self, column_name: impl Into<String>) -> Self {
        let column_name = column_name.into();

        // make a new schema with the specified column and
        // merge it in to any existing schema
        let new_column_schema = SchemaBuilder::new().tag(&column_name).build().unwrap();

        self.add_schema_to_table(new_column_schema, true, None)
    }

    /// Register a tag column with stats with the test chunk
    pub fn with_tag_column_with_stats(
        self,
        column_name: impl Into<String>,
        min: Option<&str>,
        max: Option<&str>,
    ) -> Self {
        self.with_tag_column_with_full_stats(column_name, min, max, 0, None)
    }

    /// Register a tag column with stats with the test chunk
    pub fn with_tag_column_with_full_stats(
        self,
        column_name: impl Into<String>,
        min: Option<&str>,
        max: Option<&str>,
        count: u64,
        distinct_count: Option<NonZeroU64>,
    ) -> Self {
        let null_count = 0;
        self.with_tag_column_with_nulls_and_full_stats(
            column_name,
            min,
            max,
            count,
            distinct_count,
            null_count,
        )
    }

    /// Register a tag column with stats with the test chunk
    pub fn with_tag_column_with_nulls_and_full_stats(
        self,
        column_name: impl Into<String>,
        min: Option<&str>,
        max: Option<&str>,
        count: u64,
        distinct_count: Option<NonZeroU64>,
        null_count: u64,
    ) -> Self {
        let column_name = column_name.into();

        // make a new schema with the specified column and
        // merge it in to any existing schema
        let new_column_schema = SchemaBuilder::new().tag(&column_name).build().unwrap();

        // Construct stats
        let stats = Statistics::String(StatValues {
            min: min.map(ToString::to_string),
            max: max.map(ToString::to_string),
            total_count: count,
            null_count: Some(null_count),
            distinct_count,
        });

        self.add_schema_to_table(new_column_schema, true, Some(stats))
    }

    /// Register a timestamp column with the test chunk with default stats
    pub fn with_time_column(self) -> Self {
        // make a new schema with the specified column and
        // merge it in to any existing schema
        let new_column_schema = SchemaBuilder::new().timestamp().build().unwrap();

        self.add_schema_to_table(new_column_schema, true, None)
    }

    /// Register a timestamp column with the test chunk
    pub fn with_time_column_with_stats(self, min: Option<i64>, max: Option<i64>) -> Self {
        self.with_time_column_with_full_stats(min, max, 0, None)
    }

    /// Register a timestamp column with full stats with the test chunk
    pub fn with_time_column_with_full_stats(
        mut self,
        min: Option<i64>,
        max: Option<i64>,
        count: u64,
        distinct_count: Option<NonZeroU64>,
    ) -> Self {
        // make a new schema with the specified column and
        // merge it in to any existing schema
        let new_column_schema = SchemaBuilder::new().timestamp().build().unwrap();
        let null_count = 0;

        // Construct stats
        let stats = Statistics::I64(StatValues {
            min,
            max,
            total_count: count,
            null_count: Some(null_count),
            distinct_count,
        });

        if let Some(min) = min {
            if let Some(max) = max {
                self.timestamp_min_max = Some(TimestampMinMax { min, max });
            }
        }

        self.add_schema_to_table(new_column_schema, true, Some(stats))
    }

    pub fn with_timestamp_min_max(mut self, min: i64, max: i64) -> Self {
        self.timestamp_min_max = Some(TimestampMinMax { min, max });
        self
    }

    impl_with_column!(with_i64_field_column, Int64);
    impl_with_column_no_stats!(with_i64_field_column_no_stats, Int64);
    impl_with_column_with_stats!(with_i64_field_column_with_stats, Int64, i64, I64);

    impl_with_column!(with_u64_column, UInt64);
    impl_with_column_no_stats!(with_u64_field_column_no_stats, UInt64);
    impl_with_column_with_stats!(with_u64_field_column_with_stats, UInt64, u64, U64);

    impl_with_column!(with_f64_field_column, Float64);
    impl_with_column_no_stats!(with_f64_field_column_no_stats, Float64);
    impl_with_column_with_stats!(with_f64_field_column_with_stats, Float64, f64, F64);

    impl_with_column!(with_bool_field_column, Boolean);
    impl_with_column_no_stats!(with_bool_field_column_no_stats, Boolean);
    impl_with_column_with_stats!(with_bool_field_column_with_stats, Boolean, bool, Bool);

    /// Register a string field column with the test chunk
    pub fn with_string_field_column_with_stats(
        self,
        column_name: impl Into<String>,
        min: Option<&str>,
        max: Option<&str>,
    ) -> Self {
        let column_name = column_name.into();

        // make a new schema with the specified column and
        // merge it in to any existing schema
        let new_column_schema = SchemaBuilder::new()
            .field(&column_name, DataType::Utf8)
            .build()
            .unwrap();

        // Construct stats
        let stats = Statistics::String(StatValues {
            min: min.map(ToString::to_string),
            max: max.map(ToString::to_string),
            ..Default::default()
        });

        self.add_schema_to_table(new_column_schema, true, Some(stats))
    }

    /// Adds the specified schema and optionally a column summary containing optional stats.
    /// If `add_column_summary` is false, `stats` is ignored. If `add_column_summary` is true but
    /// `stats` is `None`, default stats will be added to the column summary.
    fn add_schema_to_table(
        mut self,
        new_column_schema: Schema,
        add_column_summary: bool,
        stats: Option<Statistics>,
    ) -> Self {
        // assume the new schema has exactly a single table
        assert_eq!(new_column_schema.len(), 1);
        let (col_type, new_field) = new_column_schema.field(0);

        let mut merger = SchemaMerger::new();
        merger = merger.merge(&new_column_schema).unwrap();
        merger = merger
            .merge(self.schema.as_ref())
            .expect("merging was successful");
        self.schema = Arc::new(merger.build());

        if add_column_summary {
            let influxdb_type = col_type.map(|t| match t {
                InfluxColumnType::Tag => InfluxDbType::Tag,
                InfluxColumnType::Field(_) => InfluxDbType::Field,
                InfluxColumnType::Timestamp => InfluxDbType::Timestamp,
            });

            let stats = stats.unwrap_or_else(|| match new_field.data_type() {
                DataType::Boolean => Statistics::Bool(StatValues::default()),
                DataType::Int64 => Statistics::I64(StatValues::default()),
                DataType::UInt64 => Statistics::U64(StatValues::default()),
                DataType::Utf8 => Statistics::String(StatValues::default()),
                DataType::Dictionary(_, value_type) => {
                    assert!(matches!(**value_type, DataType::Utf8));
                    Statistics::String(StatValues::default())
                }
                DataType::Float64 => Statistics::F64(StatValues::default()),
                DataType::Timestamp(_, _) => Statistics::I64(StatValues::default()),
                _ => panic!("Unsupported type in TestChunk: {:?}", new_field.data_type()),
            });

            let column_summary = ColumnSummary {
                name: new_field.name().clone(),
                influxdb_type,
                stats,
            };

            self.table_summary.columns.push(column_summary);
        }

        self
    }

    /// Get a copy of any predicate passed to the function
    pub fn predicates(&self) -> Vec<Predicate> {
        self.predicates.lock().clone()
    }

    /// Prepares this chunk to return a specific record batch with one
    /// row of non null data.
    pub fn with_one_row_of_data(mut self) -> Self {
        // create arrays
        let columns = self
            .schema
            .iter()
            .map(|(_influxdb_column_type, field)| match field.data_type() {
                DataType::Int64 => Arc::new(Int64Array::from(vec![1000])) as ArrayRef,
                DataType::Utf8 => Arc::new(StringArray::from(vec!["MA"])) as ArrayRef,
                DataType::Timestamp(TimeUnit::Nanosecond, _) => {
                    Arc::new(TimestampNanosecondArray::from_vec(vec![1000], None)) as ArrayRef
                }
                DataType::Dictionary(key, value)
                    if key.as_ref() == &DataType::Int32 && value.as_ref() == &DataType::Utf8 =>
                {
                    let dict: DictionaryArray<Int32Type> = vec!["MA"].into_iter().collect();
                    Arc::new(dict) as ArrayRef
                }
                _ => unimplemented!(
                    "Unimplemented data type for test database: {:?}",
                    field.data_type()
                ),
            })
            .collect::<Vec<_>>();

        let batch =
            RecordBatch::try_new(self.schema.as_ref().into(), columns).expect("made record batch");
        println!("TestChunk batch data: {:#?}", batch);

        self.table_data.push(Arc::new(batch));
        self
    }

    /// Prepares this chunk to return a specific record batch with three
    /// rows of non null data that look like, no duplicates within
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| tag1 | tag2 | field_int | time                          |",
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| WA   | SC   | 1000      | 1970-01-01 00:00:00.000008    |",
    ///   "| VT   | NC   | 10        | 1970-01-01 00:00:00.000010    |",
    ///   "| UT   | RI   | 70        | 1970-01-01 00:00:00.000020    |",
    ///   "+------+------+-----------+-------------------------------+",
    /// Stats(min, max) : tag1(UT, WA), tag2(RI, SC), time(8000, 20000)
    pub fn with_three_rows_of_data(mut self) -> Self {
        // create arrays
        let columns = self
            .schema
            .iter()
            .map(|(_influxdb_column_type, field)| match field.data_type() {
                DataType::Int64 => Arc::new(Int64Array::from(vec![1000, 10, 70])) as ArrayRef,
                DataType::UInt64 => Arc::new(UInt64Array::from(vec![1000, 10, 70])) as ArrayRef,
                DataType::Utf8 => match field.name().as_str() {
                    "tag1" => Arc::new(StringArray::from(vec!["WA", "VT", "UT"])) as ArrayRef,
                    "tag2" => Arc::new(StringArray::from(vec!["SC", "NC", "RI"])) as ArrayRef,
                    _ => Arc::new(StringArray::from(vec!["TX", "PR", "OR"])) as ArrayRef,
                },
                DataType::Timestamp(TimeUnit::Nanosecond, _) => Arc::new(
                    TimestampNanosecondArray::from_vec(vec![8000, 10000, 20000], None),
                ) as ArrayRef,
                DataType::Dictionary(key, value)
                    if key.as_ref() == &DataType::Int32 && value.as_ref() == &DataType::Utf8 =>
                {
                    match field.name().as_str() {
                        "tag1" => Arc::new(
                            vec!["WA", "VT", "UT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        "tag2" => Arc::new(
                            vec!["SC", "NC", "RI"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        _ => Arc::new(
                            vec!["TX", "PR", "OR"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                    }
                }
                _ => unimplemented!(
                    "Unimplemented data type for test database: {:?}",
                    field.data_type()
                ),
            })
            .collect::<Vec<_>>();

        let batch =
            RecordBatch::try_new(self.schema.as_ref().into(), columns).expect("made record batch");

        self.table_data.push(Arc::new(batch));
        self
    }

    /// Prepares this chunk to return a specific record batch with four
    /// rows of non null data that look like, duplicates within
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| tag1 | tag2 | field_int | time                          |",
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| WA   | SC   | 1000      | 1970-01-01 00:00:00.000028    |",
    ///   "| VT   | NC   | 10        | 1970-01-01 00:00:00.000210    |", (1)
    ///   "| UT   | RI   | 70        | 1970-01-01 00:00:00.000220    |",
    ///   "| VT   | NC   | 50        | 1970-01-01 00:00:00.000210    |", // duplicate of (1)
    ///   "+------+------+-----------+-------------------------------+",
    /// Stats(min, max) : tag1(UT, WA), tag2(RI, SC), time(28000, 220000)
    pub fn with_four_rows_of_data(mut self) -> Self {
        // create arrays
        let columns = self
            .schema
            .iter()
            .map(|(_influxdb_column_type, field)| match field.data_type() {
                DataType::Int64 => Arc::new(Int64Array::from(vec![1000, 10, 70, 50])) as ArrayRef,
                DataType::Utf8 => match field.name().as_str() {
                    "tag1" => Arc::new(StringArray::from(vec!["WA", "VT", "UT", "VT"])) as ArrayRef,
                    "tag2" => Arc::new(StringArray::from(vec!["SC", "NC", "RI", "NC"])) as ArrayRef,
                    _ => Arc::new(StringArray::from(vec!["TX", "PR", "OR", "AL"])) as ArrayRef,
                },
                DataType::Timestamp(TimeUnit::Nanosecond, _) => Arc::new(
                    TimestampNanosecondArray::from_vec(vec![28000, 210000, 220000, 210000], None),
                ) as ArrayRef,
                DataType::Dictionary(key, value)
                    if key.as_ref() == &DataType::Int32 && value.as_ref() == &DataType::Utf8 =>
                {
                    match field.name().as_str() {
                        "tag1" => Arc::new(
                            vec!["WA", "VT", "UT", "VT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        "tag2" => Arc::new(
                            vec!["SC", "NC", "RI", "NC"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        _ => Arc::new(
                            vec!["TX", "PR", "OR", "AL"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                    }
                }
                _ => unimplemented!(
                    "Unimplemented data type for test database: {:?}",
                    field.data_type()
                ),
            })
            .collect::<Vec<_>>();

        let batch =
            RecordBatch::try_new(self.schema.as_ref().into(), columns).expect("made record batch");

        self.table_data.push(Arc::new(batch));
        self
    }

    /// Prepares this chunk to return a specific record batch with five
    /// rows of non null data that look like, no duplicates within
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| tag1 | tag2 | field_int | time                          |",
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| MT   | CT   | 1000      | 1970-01-01 00:00:00.000001    |",
    ///   "| MT   | AL   | 10        | 1970-01-01 00:00:00.000007    |",
    ///   "| CT   | CT   | 70        | 1970-01-01 00:00:00.000000100 |",
    ///   "| AL   | MA   | 100       | 1970-01-01 00:00:00.000000050 |",
    ///   "| MT   | AL   | 5         | 1970-01-01 00:00:00.000005    |",
    ///   "+------+------+-----------+-------------------------------+",
    /// Stats(min, max) : tag1(AL, MT), tag2(AL, MA), time(5, 7000)
    pub fn with_five_rows_of_data(mut self) -> Self {
        // create arrays
        let columns = self
            .schema
            .iter()
            .map(|(_influxdb_column_type, field)| match field.data_type() {
                DataType::Int64 => {
                    Arc::new(Int64Array::from(vec![1000, 10, 70, 100, 5])) as ArrayRef
                }
                DataType::Utf8 => {
                    match field.name().as_str() {
                        "tag1" => Arc::new(StringArray::from(vec!["MT", "MT", "CT", "AL", "MT"]))
                            as ArrayRef,
                        "tag2" => Arc::new(StringArray::from(vec!["CT", "AL", "CT", "MA", "AL"]))
                            as ArrayRef,
                        _ => Arc::new(StringArray::from(vec!["CT", "MT", "AL", "AL", "MT"]))
                            as ArrayRef,
                    }
                }
                DataType::Timestamp(TimeUnit::Nanosecond, _) => Arc::new(
                    TimestampNanosecondArray::from_vec(vec![1000, 7000, 100, 50, 5000], None),
                ) as ArrayRef,
                DataType::Dictionary(key, value)
                    if key.as_ref() == &DataType::Int32 && value.as_ref() == &DataType::Utf8 =>
                {
                    match field.name().as_str() {
                        "tag1" => Arc::new(
                            vec!["MT", "MT", "CT", "AL", "MT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        "tag2" => Arc::new(
                            vec!["CT", "AL", "CT", "MA", "AL"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        _ => Arc::new(
                            vec!["CT", "MT", "AL", "AL", "MT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                    }
                }
                _ => unimplemented!(
                    "Unimplemented data type for test database: {:?}",
                    field.data_type()
                ),
            })
            .collect::<Vec<_>>();

        let batch =
            RecordBatch::try_new(self.schema.as_ref().into(), columns).expect("made record batch");

        self.table_data.push(Arc::new(batch));
        self
    }

    /// Prepares this chunk to return a specific record batch with ten
    /// rows of non null data that look like, duplicates within
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| tag1 | tag2 | field_int | time                          |",
    ///   "+------+------+-----------+-------------------------------+",
    ///   "| MT   | CT   | 1000      | 1970-01-01 00:00:00.000001    |",
    ///   "| MT   | AL   | 10        | 1970-01-01 00:00:00.000007    |", (1)
    ///   "| CT   | CT   | 70        | 1970-01-01 00:00:00.000000100 |",
    ///   "| AL   | MA   | 100       | 1970-01-01 00:00:00.000000050 |", (2)
    ///   "| MT   | AL   | 5         | 1970-01-01 00:00:00.000005    |", (3)
    ///   "| MT   | CT   | 1000      | 1970-01-01 00:00:00.000002    |",
    ///   "| MT   | AL   | 20        | 1970-01-01 00:00:00.000007    |",  // Duplicate with (1)
    ///   "| CT   | CT   | 70        | 1970-01-01 00:00:00.000000500 |",
    ///   "| AL   | MA   | 10        | 1970-01-01 00:00:00.000000050 |",  // Duplicate with (2)
    ///   "| MT   | AL   | 30        | 1970-01-01 00:00:00.000005    |",  // Duplicate with (3)
    ///   "+------+------+-----------+-------------------------------+",
    /// Stats(min, max) : tag1(AL, MT), tag2(AL, MA), time(5, 7000)
    pub fn with_ten_rows_of_data_some_duplicates(mut self) -> Self {
        // create arrays
        let columns = self
            .schema
            .iter()
            .map(|(_influxdb_column_type, field)| match field.data_type() {
                DataType::Int64 => Arc::new(Int64Array::from(vec![
                    1000, 10, 70, 100, 5, 1000, 20, 70, 10, 30,
                ])) as ArrayRef,
                DataType::Utf8 => match field.name().as_str() {
                    "tag1" => Arc::new(StringArray::from(vec![
                        "MT", "MT", "CT", "AL", "MT", "MT", "MT", "CT", "AL", "MT",
                    ])) as ArrayRef,
                    "tag2" => Arc::new(StringArray::from(vec![
                        "CT", "AL", "CT", "MA", "AL", "CT", "AL", "CT", "MA", "AL",
                    ])) as ArrayRef,
                    _ => Arc::new(StringArray::from(vec![
                        "CT", "MT", "AL", "AL", "MT", "CT", "MT", "AL", "AL", "MT",
                    ])) as ArrayRef,
                },
                DataType::Timestamp(TimeUnit::Nanosecond, _) => {
                    Arc::new(TimestampNanosecondArray::from_vec(
                        vec![1000, 7000, 100, 50, 5, 2000, 7000, 500, 50, 5],
                        None,
                    )) as ArrayRef
                }
                DataType::Dictionary(key, value)
                    if key.as_ref() == &DataType::Int32 && value.as_ref() == &DataType::Utf8 =>
                {
                    match field.name().as_str() {
                        "tag1" => Arc::new(
                            vec!["MT", "MT", "CT", "AL", "MT", "MT", "MT", "CT", "AL", "MT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        "tag2" => Arc::new(
                            vec!["CT", "AL", "CT", "MA", "AL", "CT", "AL", "CT", "MA", "AL"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                        _ => Arc::new(
                            vec!["CT", "MT", "AL", "AL", "MT", "CT", "MT", "AL", "AL", "MT"]
                                .into_iter()
                                .collect::<DictionaryArray<Int32Type>>(),
                        ) as ArrayRef,
                    }
                }
                _ => unimplemented!(
                    "Unimplemented data type for test database: {:?}",
                    field.data_type()
                ),
            })
            .collect::<Vec<_>>();

        let batch =
            RecordBatch::try_new(self.schema.as_ref().into(), columns).expect("made record batch");

        self.table_data.push(Arc::new(batch));
        self
    }

    /// Set the sort key for this chunk
    pub fn with_sort_key(self, sort_key: SortKey) -> Self {
        Self {
            sort_key: Some(sort_key),
            ..self
        }
    }

    /// Set the partition sort key for this chunk
    pub fn with_partition_sort_key(self, sort_key: SortKey) -> Self {
        Self {
            partition_sort_key: Some(sort_key),
            ..self
        }
    }

    /// Returns all columns of the table
    pub fn all_column_names(&self) -> StringSet {
        self.schema
            .iter()
            .map(|(_, field)| field.name().to_string())
            .collect()
    }

    /// Returns just the specified columns
    pub fn specific_column_names_selection(&self, columns: &[&str]) -> StringSet {
        self.schema
            .iter()
            .map(|(_, field)| field.name().to_string())
            .filter(|col| columns.contains(&col.as_str()))
            .collect()
    }
}

impl fmt::Display for TestChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.table_name())
    }
}

impl QueryChunk for TestChunk {
    fn id(&self) -> ChunkId {
        self.id
    }

    fn table_name(&self) -> &str {
        &self.table_name
    }

    fn may_contain_pk_duplicates(&self) -> bool {
        self.may_contain_pk_duplicates
    }

    fn read_filter(
        &self,
        _ctx: IOxSessionContext,
        predicate: &Predicate,
        _selection: Selection<'_>,
    ) -> Result<SendableRecordBatchStream, QueryChunkError> {
        self.check_error()?;

        // save the predicate
        self.predicates.lock().push(predicate.clone());

        let batches = self.table_data.clone();
        Ok(stream_from_batches(batches))
    }

    fn chunk_type(&self) -> &str {
        "Test Chunk"
    }

    fn apply_predicate_to_metadata(
        &self,
        predicate: &Predicate,
    ) -> Result<PredicateMatch, QueryChunkError> {
        self.check_error()?;

        // save the predicate
        self.predicates.lock().push(predicate.clone());

        // check if there is a saved result to return
        if let Some(&predicate_match) = self.predicate_match.as_ref() {
            return Ok(predicate_match);
        }

        Ok(PredicateMatch::Unknown)
    }

    fn column_values(
        &self,
        _ctx: IOxSessionContext,
        _column_name: &str,
        _predicate: &Predicate,
    ) -> Result<Option<StringSet>, QueryChunkError> {
        // Model not being able to get column values from metadata
        Ok(None)
    }

    fn column_names(
        &self,
        _ctx: IOxSessionContext,
        predicate: &Predicate,
        selection: Selection<'_>,
    ) -> Result<Option<StringSet>, QueryChunkError> {
        self.check_error()?;

        // save the predicate
        self.predicates.lock().push(predicate.clone());

        // only return columns specified in selection
        let column_names = match selection {
            Selection::All => self.all_column_names(),
            Selection::Some(cols) => self.specific_column_names_selection(cols),
        };

        Ok(Some(column_names))
    }

    fn order(&self) -> ChunkOrder {
        self.order
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl QueryChunkMeta for TestChunk {
    fn summary(&self) -> Option<Arc<TableSummary>> {
        Some(Arc::new(self.table_summary.clone()))
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn partition_sort_key(&self) -> Option<&SortKey> {
        self.partition_sort_key.as_ref()
    }

    fn partition_id(&self) -> Option<PartitionId> {
        self.partition_id
    }

    fn sort_key(&self) -> Option<&SortKey> {
        self.sort_key.as_ref()
    }

    // return a reference to delete predicates of the chunk
    fn delete_predicates(&self) -> &[Arc<DeletePredicate>] {
        let pred = &self.delete_predicates;
        debug!(?pred, "Delete predicate in Test Chunk");

        pred
    }

    fn timestamp_min_max(&self) -> Option<TimestampMinMax> {
        self.timestamp_min_max
    }
}

/// Return the raw data from the list of chunks
pub async fn raw_data(chunks: &[Arc<dyn QueryChunk>]) -> Vec<RecordBatch> {
    let mut batches = vec![];
    for c in chunks {
        let pred = Predicate::default();
        let selection = Selection::All;
        let mut stream = c
            .read_filter(IOxSessionContext::with_testing(), &pred, selection)
            .expect("Error in read_filter");
        while let Some(b) = stream.next().await {
            let b = b.expect("Error in stream");
            batches.push(b)
        }
    }
    batches
}
