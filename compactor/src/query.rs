//! Queryable Compactor Data

use data_types::{
    ChunkId, ChunkOrder, CompactionLevel, DeletePredicate, PartitionId, SequenceNumber,
    TableSummary, Timestamp, TimestampMinMax, Tombstone,
};
use datafusion::physical_plan::SendableRecordBatchStream;
use iox_query::{
    exec::{stringset::StringSet, IOxSessionContext},
    QueryChunk, QueryChunkError, QueryChunkMeta,
};
use observability_deps::tracing::trace;
use parquet_file::chunk::ParquetChunk;
use predicate::{delete_predicate::tombstones_to_delete_predicates, Predicate};
use schema::{merge::SchemaMerger, selection::Selection, sort::SortKey, Schema};
use snafu::{ResultExt, Snafu};
use std::{any::Any, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub enum Error {
    #[snafu(display("Failed to read parquet: {}", source))]
    ReadParquet {
        source: parquet_file::storage::ReadError,
    },

    #[snafu(display(
        "Error reading IOx Metadata from Parquet IoxParquetMetadata: {}",
        source
    ))]
    ReadParquetMeta {
        source: parquet_file::storage::ReadError,
    },
}

/// A specialized `Error` for Compactor's query errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// QueryableParquetChunk that implements QueryChunk and QueryMetaChunk for building query plan
#[derive(Debug, Clone)]
pub struct QueryableParquetChunk {
    data: Arc<ParquetChunk>,                      // data of the parquet file
    delete_predicates: Vec<Arc<DeletePredicate>>, // converted from tombstones
    table_name: String,                           // needed to build query plan
    partition_id: PartitionId,
    max_sequence_number: SequenceNumber,
    min_time: Timestamp,
    max_time: Timestamp,
    sort_key: Option<SortKey>,
    partition_sort_key: Option<SortKey>,
    compaction_level: CompactionLevel,
}

impl QueryableParquetChunk {
    /// Initialize a QueryableParquetChunk
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table_name: impl Into<String>,
        partition_id: PartitionId,
        data: Arc<ParquetChunk>,
        deletes: &[Tombstone],
        max_sequence_number: SequenceNumber,
        min_time: Timestamp,
        max_time: Timestamp,
        sort_key: Option<SortKey>,
        partition_sort_key: Option<SortKey>,
        compaction_level: CompactionLevel,
    ) -> Self {
        let delete_predicates = tombstones_to_delete_predicates(deletes);
        Self {
            data,
            delete_predicates,
            table_name: table_name.into(),
            partition_id,
            max_sequence_number,
            min_time,
            max_time,
            sort_key,
            partition_sort_key,
            compaction_level,
        }
    }

    /// Merge schema of the given chunks
    pub fn merge_schemas(chunks: &[Arc<dyn QueryChunk>]) -> Arc<Schema> {
        let mut merger = SchemaMerger::new();
        for chunk in chunks {
            merger = merger.merge(&chunk.schema()).expect("schemas compatible");
        }
        Arc::new(merger.build())
    }

    /// Return max sequence number
    pub fn max_sequence_number(&self) -> SequenceNumber {
        self.max_sequence_number
    }

    /// Return min time
    pub fn min_time(&self) -> i64 {
        self.min_time.get()
    }

    /// Return max time
    pub fn max_time(&self) -> i64 {
        self.max_time.get()
    }

    /// Return the parquet file's object store id
    pub fn object_store_id(&self) -> Uuid {
        self.data.object_store_id()
    }
}

impl QueryChunkMeta for QueryableParquetChunk {
    fn summary(&self) -> Option<Arc<TableSummary>> {
        None
    }

    fn schema(&self) -> Arc<Schema> {
        self.data.schema()
    }

    fn partition_sort_key(&self) -> Option<&SortKey> {
        self.partition_sort_key.as_ref()
    }

    fn partition_id(&self) -> Option<PartitionId> {
        Some(self.partition_id)
    }

    fn sort_key(&self) -> Option<&SortKey> {
        self.sort_key.as_ref()
    }

    fn delete_predicates(&self) -> &[Arc<DeletePredicate>] {
        self.delete_predicates.as_ref()
    }

    fn timestamp_min_max(&self) -> Option<TimestampMinMax> {
        Some(TimestampMinMax {
            min: self.min_time(),
            max: self.max_time(),
        })
    }
}

impl QueryChunk for QueryableParquetChunk {
    // This function is needed to distinguish the ParquetChunks further if they happen to have the
    // same creation order.
    // Ref: chunks.sort_unstable_by_key(|c| (c.order(), c.id())); in provider.rs
    // Note: The order of this QueryableParquetChunk is the parquet file's min_sequence_number which
    // will be the same for parquet files of splitted compacted data.
    //
    // This function returns the parquet file's min_time which will be always different for the
    // parquet files of same order/min_sequence_number and is good to order the parquet file
    fn id(&self) -> ChunkId {
        // When we need the order to split overlapped chunks, the ChunkOrder is already different.
        // ChunkId is used as tiebreaker does not matter much, so use the object store id
        self.object_store_id().into()
    }

    /// Returns the name of the table stored in this chunk
    fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns true if the chunk may contain a duplicate "primary
    /// key" within itself
    fn may_contain_pk_duplicates(&self) -> bool {
        // data within this parquet chunk was deduplicated
        false
    }

    /// Returns a set of Strings with column names from the specified
    /// table that have at least one row that matches `predicate`, if
    /// the predicate can be evaluated entirely on the metadata of
    /// this Chunk. Returns `None` otherwise
    fn column_names(
        &self,
        _ctx: IOxSessionContext,
        _predicate: &Predicate,
        _columns: Selection<'_>,
    ) -> Result<Option<StringSet>, QueryChunkError> {
        Ok(None)
    }

    /// Return a set of Strings containing the distinct values in the
    /// specified columns. If the predicate can be evaluated entirely
    /// on the metadata of this Chunk. Returns `None` otherwise
    ///
    /// The requested columns must all have String type.
    fn column_values(
        &self,
        _ctx: IOxSessionContext,
        _column_name: &str,
        _predicate: &Predicate,
    ) -> Result<Option<StringSet>, QueryChunkError> {
        Ok(None)
    }

    /// Provides access to raw `QueryChunk` data as an
    /// asynchronous stream of `RecordBatch`es filtered by a *required*
    /// predicate. Note that not all chunks can evaluate all types of
    /// predicates and this function will return an error
    /// if requested to evaluate with a predicate that is not supported
    ///
    /// This is the analog of the `TableProvider` in DataFusion
    ///
    /// The reason we can't simply use the `TableProvider` trait
    /// directly is that the data for a particular Table lives in
    /// several chunks within a partition, so there needs to be an
    /// implementation of `TableProvider` that stitches together the
    /// streams from several different `QueryChunk`s.
    fn read_filter(
        &self,
        mut ctx: IOxSessionContext,
        predicate: &Predicate,
        selection: Selection<'_>,
    ) -> Result<SendableRecordBatchStream, QueryChunkError> {
        ctx.set_metadata("storage", "compactor");
        ctx.set_metadata("projection", format!("{}", selection));
        trace!(?selection, "selection");

        self.data
            .read_filter(predicate, selection)
            .context(ReadParquetSnafu)
            .map_err(|e| Box::new(e) as _)
    }

    /// Returns chunk type
    fn chunk_type(&self) -> &str {
        "QueryableParquetChunk"
    }

    // Order of the chunk so they can be deduplicate correctly
    fn order(&self) -> ChunkOrder {
        match self.compaction_level {
            CompactionLevel::Initial => ChunkOrder::new(self.max_sequence_number.get()),
            CompactionLevel::FileNonOverlapped => ChunkOrder::new(0),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_types::ColumnType;
    use iox_tests::util::{TestCatalog, TestParquetFileBuilder};
    use parquet_file::storage::ParquetStorage;

    async fn test_setup(
        compaction_level: CompactionLevel,
        max_sequence_number: i64,
    ) -> QueryableParquetChunk {
        let catalog = TestCatalog::new();
        let ns = catalog.create_namespace("ns").await;
        let sequencer = ns.create_sequencer(1).await;
        let table = ns.create_table("table").await;
        table.create_column("field_int", ColumnType::I64).await;
        table.create_column("tag1", ColumnType::Tag).await;
        table.create_column("time", ColumnType::Time).await;

        let partition = table
            .with_sequencer(&sequencer)
            .create_partition("2022-07-13")
            .await;

        let lp = vec!["table,tag1=WA field_int=1000i 8000"].join("\n");
        let builder = TestParquetFileBuilder::default()
            .with_line_protocol(&lp)
            .with_compaction_level(compaction_level)
            .with_max_seq(max_sequence_number);
        let file = partition.create_parquet_file(builder).await;
        let parquet_file = Arc::new(file.parquet_file);

        let parquet_chunk = Arc::new(ParquetChunk::new(
            Arc::clone(&parquet_file),
            Arc::new(table.schema().await),
            ParquetStorage::new(Arc::clone(&catalog.object_store)),
        ));

        QueryableParquetChunk::new(
            "table",
            partition.partition.id,
            parquet_chunk,
            &[],
            parquet_file.max_sequence_number,
            parquet_file.min_time,
            parquet_file.max_time,
            None,
            None,
            parquet_file.compaction_level,
        )
    }

    #[tokio::test]
    async fn chunk_order_is_max_seq_when_compaction_level_0() {
        let chunk = test_setup(CompactionLevel::Initial, 2).await;

        assert_eq!(chunk.order(), ChunkOrder::new(2));
    }

    #[tokio::test]
    async fn chunk_order_is_0_when_compaction_level_1() {
        let chunk = test_setup(CompactionLevel::FileNonOverlapped, 2).await;

        assert_eq!(chunk.order(), ChunkOrder::new(0));
    }
}
