//! This module contains util functions for testing scenarios
use super::DbScenario;
use async_trait::async_trait;
use backoff::BackoffConfig;
use data_types::{
    DeletePredicate, IngesterMapping, KafkaPartition, NonEmptyString, ParquetFileId, PartitionId,
    PartitionKey, Sequence, SequenceNumber, SequencerId, TombstoneId,
};
use dml::{DmlDelete, DmlMeta, DmlOperation, DmlWrite};
use futures::StreamExt;
use generated_types::{
    influxdata::iox::ingester::v1::{IngesterQueryResponseMetadata, PartitionStatus},
    ingester::IngesterQueryRequest,
};
use influxdb_iox_client::flight::{low_level::LowLevelMessage, Error as FlightError};
use ingester::{
    data::{
        FlatIngesterQueryResponse, IngesterData, IngesterQueryResponse, Persister, SequencerData,
    },
    lifecycle::LifecycleHandle,
    querier_handler::prepare_data_to_querier,
};
use iox_catalog::interface::get_schema_by_name;
use iox_query::exec::{Executor, ExecutorConfig};
use iox_tests::util::{TestCatalog, TestNamespace, TestSequencer};
use itertools::Itertools;
use mutable_batch_lp::LinesConverter;
use once_cell::sync::Lazy;
use parquet_file::storage::ParquetStorage;
use querier::{
    IngesterConnectionImpl, IngesterFlightClient, IngesterFlightClientError,
    IngesterFlightClientQueryData, QuerierCatalogCache, QuerierChunkLoadSetting, QuerierNamespace,
};
use schema::selection::Selection;
use sharder::JumpHash;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Display,
    fmt::Write,
    sync::Arc,
};

// Structs, enums, and functions used to exhaust all test scenarios of chunk lifecycle
// & when delete predicates are applied

// STRUCTs & ENUMs
#[derive(Debug, Clone, Default)]
pub struct ChunkData<'a, 'b> {
    /// Line protocol data of this chunk
    pub lp_lines: Vec<&'a str>,

    /// which stage this chunk will be created.
    ///
    /// If not set, this chunk will be created in [all](ChunkStage::all) stages. This can be
    /// helpful when the test scenario is not specific to the chunk stage. If this is used for
    /// multiple chunks, then all stage permutations will be generated.
    pub chunk_stage: Option<ChunkStage>,

    /// Delete predicates
    pub preds: Vec<Pred<'b>>,

    /// Table that should be deleted.
    pub delete_table_name: Option<&'a str>,

    /// Partition key
    pub partition_key: &'a str,
}

impl<'a, 'b> ChunkData<'a, 'b> {
    fn with_chunk_stage(self, chunk_stage: ChunkStage) -> Self {
        assert!(self.chunk_stage.is_none());

        Self {
            chunk_stage: Some(chunk_stage),
            ..self
        }
    }

    /// Replace [`DeleteTime::Begin`] and [`DeleteTime::End`] with values that correspond to the
    /// linked [`ChunkStage`].
    fn replace_begin_and_end_delete_times(self) -> Self {
        Self {
            preds: self
                .preds
                .into_iter()
                .map(|pred| {
                    pred.replace_begin_and_end_delete_times(
                        self.chunk_stage
                            .expect("chunk stage must be set at this point"),
                    )
                })
                .collect(),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChunkStage {
    /// In parquet file, persisted by the ingester. Now managed by the querier.
    Parquet,

    /// In parquet file, persisted by the ingester. Loaded into memory by querier.
    ReadBuffer,

    /// In parquet file, persisted by the ingester. Loaded from file into memory by querier on-demand.
    OnDemand,

    /// In ingester.
    Ingester,
}

impl Display for ChunkStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parquet => write!(f, "Parquet"),
            Self::ReadBuffer => write!(f, "ReadBuffer"),
            Self::OnDemand => write!(f, "OnDemand"),
            Self::Ingester => write!(f, "Ingester"),
        }
    }
}

impl PartialOrd for ChunkStage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            // allow multiple parquet chunks (for the same partition). sequence numbers will be
            // used for ordering.
            (
                Self::Parquet | Self::ReadBuffer | Self::OnDemand,
                Self::Parquet | Self::ReadBuffer | Self::OnDemand,
            ) => Some(Ordering::Equal),

            // "parquet" chunks are older (i.e. come earlier) than chunks that still life in the
            // ingester
            (Self::Parquet | Self::ReadBuffer | Self::OnDemand, Self::Ingester) => {
                Some(Ordering::Less)
            }
            (Self::Ingester, Self::Parquet | Self::ReadBuffer | Self::OnDemand) => {
                Some(Ordering::Greater)
            }

            // it's impossible for two chunks (for the same partition) to be in the ingester stage
            (Self::Ingester, Self::Ingester) => None,
        }
    }
}

impl ChunkStage {
    /// return the list of all chunk types
    pub fn all() -> Vec<Self> {
        vec![
            Self::Parquet,
            Self::ReadBuffer,
            Self::OnDemand,
            Self::Ingester,
        ]
    }
}

#[derive(Debug, Clone)]
pub struct Pred<'a> {
    /// Delete predicate
    pub predicate: &'a DeletePredicate,

    /// At which chunk stage this predicate is applied
    pub delete_time: DeleteTime,
}

impl<'a> Pred<'a> {
    /// Replace [`DeleteTime::Begin`] and [`DeleteTime::End`] with values that correspond to the
    /// linked [`ChunkStage`].
    fn replace_begin_and_end_delete_times(self, stage: ChunkStage) -> Self {
        Self {
            delete_time: self.delete_time.replace_begin_and_end_delete_times(stage),
            ..self
        }
    }

    fn new(predicate: &'a DeletePredicate, delete_time: DeleteTime) -> Self {
        Pred {
            predicate,
            delete_time,
        }
    }
}

/// Describes when a delete predicate was applied.
///
/// # Ordering
///
/// Compared to [`ChunkStage`], the ordering here may seem a bit confusing. While the latest
/// payload / LP data resists in the ingester and is not yet available as a parquet file, the
/// latest tombstones apply to parquet files and were (past tense!) NOT applied while the LP data
/// was in the ingester.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeleteTime {
    /// Special delete time which marks the first time that could be used from deletion.
    ///
    /// May depend on [`ChunkStage`].
    Begin,

    /// Delete predicate is added while chunk is was still in ingester memory.
    Ingester {
        /// Flag if the tombstone also exists in the catalog.
        ///
        /// If this is set to `false`, then the tombstone was applied by the ingester but does not
        /// exist in the catalog any longer. This can be because:
        ///
        /// - the ingester decided that it doesn't need to be added to the catalog (this is
        ///   currently/2022-04-21 not implemented!)
        /// - the compactor pruned the tombstone from the catalog because there are zero affected
        ///   parquet files
        also_in_catalog: bool,
    },

    /// Delete predicate is added to chunks at their parquet stage
    Parquet,

    /// Special delete time which marks the last time that could be used from deletion.
    ///
    /// May depend on [`ChunkStage`].
    End,
}

impl DeleteTime {
    /// Return all DeleteTime at and after the given chunk stage
    pub fn all_from_and_before(chunk_stage: ChunkStage) -> Vec<DeleteTime> {
        match chunk_stage {
            ChunkStage::Parquet | ChunkStage::ReadBuffer | ChunkStage::OnDemand => vec![
                Self::Ingester {
                    also_in_catalog: true,
                },
                Self::Ingester {
                    also_in_catalog: false,
                },
                Self::Parquet,
            ],
            ChunkStage::Ingester => vec![
                Self::Ingester {
                    also_in_catalog: true,
                },
                Self::Ingester {
                    also_in_catalog: false,
                },
            ],
        }
    }

    /// Replace [`DeleteTime::Begin`] and [`DeleteTime::End`] with values that correspond to the
    /// linked [`ChunkStage`].
    fn replace_begin_and_end_delete_times(self, stage: ChunkStage) -> Self {
        match self {
            Self::Begin => Self::begin_for(stage),
            Self::End => Self::end_for(stage),
            other @ (Self::Ingester { .. } | Self::Parquet) => other,
        }
    }

    fn begin_for(_stage: ChunkStage) -> Self {
        Self::Ingester {
            also_in_catalog: true,
        }
    }

    fn end_for(stage: ChunkStage) -> Self {
        match stage {
            ChunkStage::Ingester => Self::Ingester {
                also_in_catalog: true,
            },
            ChunkStage::Parquet | ChunkStage::ReadBuffer | ChunkStage::OnDemand => Self::Parquet,
        }
    }
}

impl Display for DeleteTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin => write!(f, "Begin"),
            Self::Ingester {
                also_in_catalog: false,
            } => write!(f, "Ingester w/o catalog entry"),
            Self::Ingester {
                also_in_catalog: true,
            } => write!(f, "Ingester w/ catalog entry"),
            Self::Parquet => write!(f, "Parquet"),
            Self::End => write!(f, "End"),
        }
    }
}

// --------------------------------------------------------------------------------------------

/// All scenarios of chunk stages and their lifecycle moves for a given set of delete predicates.
/// If the delete predicates are empty, all scenarios of different chunk stages will be returned.
pub async fn all_scenarios_for_one_chunk(
    // These delete predicates are applied at all stages of the chunk lifecycle
    chunk_stage_preds: Vec<&DeletePredicate>,
    // These delete predicates are applied to all chunks at their final stages
    at_end_preds: Vec<&DeletePredicate>,
    // Input data, formatted as line protocol. One chunk will be created for each measurement
    // (table) that appears in the input
    lp_lines: Vec<&str>,
    // Table to which the delete predicates will be applied
    delete_table_name: &str,
    // Partition of the chunk
    partition_key: &str,
) -> Vec<DbScenario> {
    let mut scenarios = vec![];
    // Go over chunk stages
    for chunk_stage in ChunkStage::all() {
        // Apply delete chunk_stage_preds to this chunk stage at all stages at and before that in
        // the lifecycle of the chunk. But we only need to get all delete times if
        // chunk_stage_preds is not empty, otherwise, produce only one scenario of each chunk stage
        let mut delete_times = vec![DeleteTime::begin_for(chunk_stage)];
        if !chunk_stage_preds.is_empty() {
            delete_times = DeleteTime::all_from_and_before(chunk_stage)
        };

        // Make delete predicates that happen when all chunks in their final stages
        let end_preds: Vec<Pred> = at_end_preds
            .iter()
            .map(|p| Pred::new(*p, DeleteTime::end_for(chunk_stage)))
            .collect();

        for delete_time in delete_times {
            // make delete predicate with time it happens
            let mut preds: Vec<Pred> = chunk_stage_preds
                .iter()
                .map(|p| Pred::new(*p, delete_time))
                .collect();
            // extend at-end predicates
            preds.extend(end_preds.clone());

            // make this specific chunk stage & delete predicates scenario
            scenarios.push(
                make_chunk_with_deletes_at_different_stages(
                    lp_lines.clone(),
                    chunk_stage,
                    preds,
                    Some(delete_table_name),
                    partition_key,
                )
                .await,
            );
        }
    }

    scenarios
}

/// Build a chunk that may move with lifecycle before/after deletes. Note that the only chunk in
/// this function can be moved to different stages, and delete predicates can be applied at
/// different stages when the chunk is moved.
async fn make_chunk_with_deletes_at_different_stages(
    lp_lines: Vec<&str>,
    chunk_stage: ChunkStage,
    preds: Vec<Pred<'_>>,
    delete_table_name: Option<&str>,
    partition_key: &str,
) -> DbScenario {
    let chunk_data = ChunkData {
        lp_lines,
        chunk_stage: Some(chunk_stage),
        preds,
        delete_table_name,
        partition_key,
    };
    let mut mock_ingester = MockIngester::new().await;
    let scenario_name = make_chunk(&mut mock_ingester, chunk_data).await;

    let db = mock_ingester.into_query_namespace().await;

    DbScenario { scenario_name, db }
}

/// Load two chunks of lp data into different chunk scenarios.
pub async fn make_two_chunk_scenarios(
    partition_key: &str,
    data1: &str,
    data2: &str,
) -> Vec<DbScenario> {
    let lp_lines1: Vec<_> = data1.split('\n').collect();
    let lp_lines2: Vec<_> = data2.split('\n').collect();

    make_n_chunks_scenario(&[
        ChunkData {
            lp_lines: lp_lines1,
            partition_key,
            ..Default::default()
        },
        ChunkData {
            lp_lines: lp_lines2,
            partition_key,
            ..Default::default()
        },
    ])
    .await
}

pub async fn make_n_chunks_scenario(chunks: &[ChunkData<'_, '_>]) -> Vec<DbScenario> {
    let n_stages_unset = chunks
        .iter()
        .filter(|chunk| chunk.chunk_stage.is_none())
        .count();

    let mut scenarios = vec![];

    'stage_combinations: for stages in ChunkStage::all()
        .into_iter()
        .combinations_with_replacement(n_stages_unset)
        .flat_map(|v| v.into_iter().permutations(n_stages_unset))
        .unique()
    {
        // combine stages w/ chunks
        let chunks_orig = chunks;
        let mut chunks = Vec::with_capacity(chunks.len());
        let mut stages_it = stages.iter();
        for chunk_data in chunks_orig {
            let mut chunk_data = chunk_data.clone();

            if chunk_data.chunk_stage.is_none() {
                let chunk_stage = stages_it.next().expect("generated enough stages");
                chunk_data = chunk_data.with_chunk_stage(*chunk_stage);
            }

            let chunk_data = chunk_data.replace_begin_and_end_delete_times();

            chunks.push(chunk_data);
        }
        assert!(stages_it.next().is_none(), "generated too many stages");

        // filter out unordered stages
        let mut stage_by_partition = HashMap::<&str, Vec<ChunkStage>>::new();
        for chunk_data in &chunks {
            stage_by_partition
                .entry(chunk_data.partition_key)
                .or_default()
                .push(
                    chunk_data
                        .chunk_stage
                        .expect("Stage should be initialized by now"),
                );
        }
        for stages in stage_by_partition.values() {
            if !stages.windows(2).all(|stages| {
                stages[0]
                    .partial_cmp(&stages[1])
                    .map(|o| o.is_le())
                    .unwrap_or_default()
            }) {
                continue 'stage_combinations;
            }
        }

        // build scenario
        let mut scenario_name = format!("{} chunks:", chunks.len());
        let mut mock_ingester = MockIngester::new().await;

        for chunk_data in chunks {
            let name = make_chunk(&mut mock_ingester, chunk_data).await;

            write!(&mut scenario_name, ", {}", name).unwrap();
        }

        let db = mock_ingester.into_query_namespace().await;
        scenarios.push(DbScenario { scenario_name, db });
    }

    scenarios
}

/// Create given chunk using the given ingester.
///
/// Returns a human-readable chunk description.
async fn make_chunk(mock_ingester: &mut MockIngester, chunk: ChunkData<'_, '_>) -> String {
    let chunk_stage = chunk.chunk_stage.expect("chunk stage should be set");

    // create chunk
    match chunk_stage {
        ChunkStage::Ingester => {
            // process write
            let (op, _partition_ids) = mock_ingester
                .simulate_write_routing(&chunk.lp_lines, chunk.partition_key)
                .await;
            mock_ingester.buffer_operation(op).await;

            // process delete predicates
            if let Some(delete_table_name) = chunk.delete_table_name {
                for pred in &chunk.preds {
                    match pred.delete_time {
                        DeleteTime::Ingester { .. } => {
                            let op = mock_ingester
                                .simulate_delete_routing(delete_table_name, pred.predicate.clone())
                                .await;
                            mock_ingester.buffer_operation(op).await;
                        }
                        other @ DeleteTime::Parquet => {
                            panic!("Cannot have delete time '{other}' for ingester chunk")
                        }
                        DeleteTime::Begin | DeleteTime::End => {
                            unreachable!(
                                "Begin/end cases should have been replaced \
                                 with concrete instances at this point"
                            )
                        }
                    }
                }
            }
        }
        ChunkStage::Parquet | ChunkStage::ReadBuffer | ChunkStage::OnDemand => {
            // process write
            let (op, partition_ids) = mock_ingester
                .simulate_write_routing(&chunk.lp_lines, chunk.partition_key)
                .await;
            mock_ingester.buffer_operation(op).await;

            // model delete predicates that are materialized (applied) by the ingester,
            // during parquet file creation
            let mut tombstone_ids = vec![];
            if let Some(delete_table_name) = chunk.delete_table_name {
                for pred in &chunk.preds {
                    match pred.delete_time {
                        DeleteTime::Ingester { .. } => {
                            let ids_pre = mock_ingester.tombstone_ids(delete_table_name).await;

                            let op = mock_ingester
                                .simulate_delete_routing(delete_table_name, pred.predicate.clone())
                                .await;
                            mock_ingester.buffer_operation(op).await;

                            // tombstones are created immediately, need to remember their ID to
                            // handle deletion later
                            let mut tombstone_id = None;
                            for id in mock_ingester.tombstone_ids(delete_table_name).await {
                                if !ids_pre.contains(&id) {
                                    assert!(tombstone_id.is_none(), "Added multiple tombstones?!");
                                    tombstone_id = Some(id);
                                }
                            }
                            tombstone_ids.push(tombstone_id.expect("No tombstone added?!"));
                        }
                        DeleteTime::Parquet => {
                            // will be attached AFTER the chunk was created
                        }
                        DeleteTime::Begin | DeleteTime::End => {
                            unreachable!(
                                "Begin/end cases should have been replaced \
                                 with concrete instances at this point"
                            )
                        }
                    }
                }
            }

            let parquet_files = mock_ingester.persist(&partition_ids).await;

            // set up load setting for querier
            for id in parquet_files {
                let load_setting = match chunk_stage {
                    ChunkStage::Parquet => QuerierChunkLoadSetting::ParquetOnly,
                    ChunkStage::ReadBuffer => QuerierChunkLoadSetting::ReadBufferOnly,
                    ChunkStage::OnDemand => QuerierChunkLoadSetting::OnDemand,
                    ChunkStage::Ingester => unreachable!("ingester chunks should not end up here"),
                };
                mock_ingester.register_load_setting(id, load_setting);
            }

            // model post-persist delete predicates
            if let Some(delete_table_name) = chunk.delete_table_name {
                let mut id_it = tombstone_ids.iter();
                for pred in &chunk.preds {
                    match pred.delete_time {
                        DeleteTime::Ingester { also_in_catalog } => {
                            // should still have a tombstone
                            let tombstone_id = *id_it.next().unwrap();
                            mock_ingester
                                .catalog
                                .catalog
                                .repositories()
                                .await
                                .tombstones()
                                .get_by_id(tombstone_id)
                                .await
                                .unwrap()
                                .expect("tombstone should be present");

                            if !also_in_catalog {
                                mock_ingester
                                    .catalog
                                    .catalog
                                    .repositories()
                                    .await
                                    .tombstones()
                                    .remove(&[tombstone_id])
                                    .await
                                    .unwrap();
                            }
                        }
                        DeleteTime::Parquet => {
                            // create new tombstone
                            let op = mock_ingester
                                .simulate_delete_routing(delete_table_name, pred.predicate.clone())
                                .await;
                            mock_ingester.buffer_operation(op).await;
                        }
                        DeleteTime::Begin | DeleteTime::End => {
                            unreachable!(
                                "Begin/end cases should have been replaced \
                                 with concrete instances at this point"
                            )
                        }
                    }
                }
            }
        }
    }

    let mut name = format!(
        "Chunk stage={} partition={}",
        chunk_stage, chunk.partition_key
    );
    let n_preds = chunk.preds.len();
    if n_preds > 0 {
        let delete_names: Vec<_> = chunk
            .preds
            .iter()
            .map(|p| p.delete_time.to_string())
            .collect();
        write!(
            name,
            " with {} delete(s) ({})",
            n_preds,
            delete_names.join(", ")
        )
        .unwrap();
    }
    name
}

/// Ingester that can be controlled specifically for query tests.
///
/// This uses as much ingester code as possible but allows more direct control over aspects like
/// lifecycle and partioning.
#[derive(Debug)]
struct MockIngester {
    /// Test catalog state.
    catalog: Arc<TestCatalog>,

    /// Namespace used for testing.
    ns: Arc<TestNamespace>,

    /// Sequencer used for testing.
    sequencer: Arc<TestSequencer>,

    /// Memory of partition keys for certain sequence numbers.
    ///
    /// This is currently required because [`DmlWrite`] does not carry partiion information so we
    /// need to do that. In production this is not required because the router and the ingester use
    /// the same partition logic, but we need direct control over the partion key for the query
    /// tests.
    partition_keys: HashMap<SequenceNumber, String>,

    /// Ingester state.
    ingester_data: Arc<IngesterData>,

    /// Next sequence number.
    ///
    /// This is kinda a poor-mans write buffer.
    sequence_counter: i64,

    /// Load settings for to-be-created querier.
    querier_load_settings: HashMap<ParquetFileId, QuerierChunkLoadSetting>,
}

/// Query-test specific executor with static properties that may be relevant for the query optimizer and therefore may
/// change `EXPLAIN` plans.
static GLOBAL_EXEC: Lazy<Arc<Executor>> = Lazy::new(|| {
    Arc::new(Executor::new_with_config(ExecutorConfig {
        num_threads: 1,
        target_query_partitions: 4,
    }))
});

impl MockIngester {
    /// Create new empty ingester.
    async fn new() -> Self {
        let exec = Arc::clone(&GLOBAL_EXEC);
        let catalog = TestCatalog::with_exec(exec);
        let ns = catalog.create_namespace("test_db").await;
        let sequencer = ns.create_sequencer(1).await;

        let sequencers = BTreeMap::from([(
            sequencer.sequencer.id,
            SequencerData::new(
                sequencer.sequencer.kafka_partition,
                catalog.metric_registry(),
            ),
        )]);
        let ingester_data = Arc::new(IngesterData::new(
            catalog.object_store(),
            catalog.catalog(),
            sequencers,
            catalog.exec(),
            BackoffConfig::default(),
        ));

        Self {
            catalog,
            ns,
            sequencer,
            partition_keys: Default::default(),
            ingester_data,
            sequence_counter: 0,
            querier_load_settings: Default::default(),
        }
    }

    /// Buffer up [`DmlOperation`] in ingester memory.
    ///
    /// This will never persist.
    ///
    /// Takes `&self mut` because our partioning implementation does not work with concurrent
    /// access.
    async fn buffer_operation(&mut self, dml_operation: DmlOperation) {
        let lifecycle_handle = NoopLifecycleHandle {};

        let should_pause = self
            .ingester_data
            .buffer_operation(
                self.sequencer.sequencer.id,
                dml_operation,
                &lifecycle_handle,
            )
            .await
            .unwrap();
        assert!(!should_pause);
    }

    /// Persists the given set of partitions.
    ///
    /// Returns newly created parquet file IDs.
    async fn persist(&mut self, partition_ids: &[PartitionId]) -> Vec<ParquetFileId> {
        let mut result = vec![];

        for partition_id in partition_ids {
            let parquet_files_pre: HashSet<ParquetFileId> = self
                .catalog
                .catalog
                .repositories()
                .await
                .parquet_files()
                .list_by_partition_not_to_delete(*partition_id)
                .await
                .unwrap()
                .into_iter()
                .map(|f| f.id)
                .collect();

            self.ingester_data.persist(*partition_id).await;

            result.extend(
                self.catalog
                    .catalog
                    .repositories()
                    .await
                    .parquet_files()
                    .list_by_partition_not_to_delete(*partition_id)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|f| f.id)
                    .filter(|id| !parquet_files_pre.contains(id)),
            )
        }

        result.sort();
        result
    }

    /// Draws new sequence number.
    fn next_sequence_number(&mut self) -> SequenceNumber {
        let next = self.sequence_counter;
        self.sequence_counter += 1;
        SequenceNumber::new(next)
    }

    /// Simulate what the router would do when it encounters the given set of line protocol lines.
    ///
    /// In contrast to a real router we have tight control over the partition key here.
    async fn simulate_write_routing(
        &mut self,
        lp_lines: &[&str],
        partition_key: &str,
    ) -> (DmlOperation, Vec<PartitionId>) {
        // detect table names and schemas from LP lines
        let mut converter = LinesConverter::new(0);
        for lp_line in lp_lines {
            converter.write_lp(lp_line).unwrap();
        }
        let (mutable_batches, _stats) = converter.finish().unwrap();

        // set up catalog
        let tables = {
            // sort names so that IDs are deterministic
            let mut table_names: Vec<_> = mutable_batches.keys().cloned().collect();
            table_names.sort();

            let mut tables = vec![];
            for table_name in table_names {
                let table = self.ns.create_table(&table_name).await;
                tables.push(table);
            }
            tables
        };
        let mut partition_ids = vec![];
        for table in &tables {
            let partition = table
                .with_sequencer(&self.sequencer)
                .create_partition(partition_key)
                .await;
            partition_ids.push(partition.partition.id);
        }
        for table in tables {
            let schema = mutable_batches
                .get(&table.table.name)
                .unwrap()
                .schema(Selection::All)
                .unwrap();

            for (t, field) in schema.iter() {
                let t = t.unwrap();
                table.create_column(field.name(), t.into()).await;
            }
        }

        let sequence_number = self.next_sequence_number();
        self.partition_keys
            .insert(sequence_number, partition_key.to_string());
        let meta = DmlMeta::sequenced(
            Sequence::new(self.sequencer.sequencer.id.get() as u32, sequence_number),
            self.catalog.time_provider().now(),
            None,
            0,
        );
        let op = DmlOperation::Write(DmlWrite::new(
            self.ns.namespace.name.clone(),
            mutable_batches,
            Some(PartitionKey::from(partition_key)),
            meta,
        ));
        (op, partition_ids)
    }

    /// Simulates what the router would do when it encouters the given delete predicate.
    async fn simulate_delete_routing(
        &mut self,
        delete_table_name: &str,
        predicate: DeletePredicate,
    ) -> DmlOperation {
        // ensure that table for deletions is also present in the catalog
        self.ns.create_table(delete_table_name).await;

        let sequence_number = self.next_sequence_number();
        let meta = DmlMeta::sequenced(
            Sequence::new(self.sequencer.sequencer.id.get() as u32, sequence_number),
            self.catalog.time_provider().now(),
            None,
            0,
        );
        DmlOperation::Delete(DmlDelete::new(
            self.ns.namespace.name.clone(),
            predicate,
            Some(NonEmptyString::new(delete_table_name).unwrap()),
            meta,
        ))
    }

    /// Get the current set of tombstone IDs for the given table.
    async fn tombstone_ids(&self, table_name: &str) -> Vec<TombstoneId> {
        let table_id = self
            .catalog
            .catalog
            .repositories()
            .await
            .tables()
            .create_or_get(table_name, self.ns.namespace.id)
            .await
            .unwrap()
            .id;
        let tombstones = self
            .catalog
            .catalog
            .repositories()
            .await
            .tombstones()
            .list_by_table(table_id)
            .await
            .unwrap();
        tombstones.iter().map(|t| t.id).collect()
    }

    /// Register load setting for given parquet file.
    fn register_load_setting(
        &mut self,
        parquet_file_id: ParquetFileId,
        load_setting: QuerierChunkLoadSetting,
    ) {
        let existing = self
            .querier_load_settings
            .insert(parquet_file_id, load_setting);
        assert!(existing.is_none());
    }

    /// Finalizes the ingester and creates a querier namespace that can be used for query tests.
    ///
    /// The querier namespace will hold a simulated connection to the ingester to be able to query
    /// unpersisted data.
    async fn into_query_namespace(self) -> Arc<QuerierNamespace> {
        let mut repos = self.catalog.catalog.repositories().await;
        let schema = Arc::new(
            get_schema_by_name(&self.ns.namespace.name, repos.as_mut())
                .await
                .unwrap(),
        );

        let catalog = Arc::clone(&self.catalog);
        let ns = Arc::clone(&self.ns);
        let catalog_cache = Arc::new(QuerierCatalogCache::new_testing(
            self.catalog.catalog(),
            self.catalog.time_provider(),
            self.catalog.metric_registry(),
        ));
        let sequencer_to_ingesters = [(0, IngesterMapping::Addr(Arc::from("some_address")))]
            .into_iter()
            .collect();
        let load_settings = self.querier_load_settings.clone();
        let ingester_connection = IngesterConnectionImpl::by_sequencer_with_flight_client(
            sequencer_to_ingesters,
            Arc::new(self),
            Arc::clone(&catalog_cache),
        );
        let ingester_connection = Arc::new(ingester_connection);
        let sharder =
            Arc::new(JumpHash::new((0..1).map(KafkaPartition::new).map(Arc::new)).unwrap());

        Arc::new(QuerierNamespace::new_testing(
            catalog_cache,
            ParquetStorage::new(catalog.object_store()),
            catalog.metric_registry(),
            ns.namespace.name.clone().into(),
            schema,
            catalog.exec(),
            Some(ingester_connection),
            sharder,
            load_settings,
            usize::MAX,
        ))
    }
}

/// Special [`LifecycleHandle`] that never persists and always accepts more data.
///
/// This is useful to control persists manually.
struct NoopLifecycleHandle {}

impl LifecycleHandle for NoopLifecycleHandle {
    fn log_write(
        &self,
        _partition_id: PartitionId,
        _sequencer_id: SequencerId,
        _sequence_number: SequenceNumber,
        _bytes_written: usize,
    ) -> bool {
        // do NOT pause ingest
        false
    }

    fn can_resume_ingest(&self) -> bool {
        true
    }
}

#[async_trait]
impl IngesterFlightClient for MockIngester {
    async fn query(
        &self,
        _ingester_address: Arc<str>,
        request: IngesterQueryRequest,
    ) -> Result<Box<dyn IngesterFlightClientQueryData>, IngesterFlightClientError> {
        // NOTE: we MUST NOT unwrap errors here because some query tests assert error behavior
        // (e.g. passing predicates of wrong types)
        let request = Arc::new(request);
        let response = prepare_data_to_querier(&self.ingester_data, &request)
            .await
            .map_err(|e| IngesterFlightClientError::Flight {
                source: FlightError::ArrowError(arrow::error::ArrowError::ExternalError(Box::new(
                    e,
                ))),
            })?;

        Ok(Box::new(QueryDataAdapter::new(response).await))
    }
}

/// Helper struct to present [`IngesterQueryResponse`] (produces by the ingester) as a
/// [`IngesterFlightClientQueryData`] (used by the querier) without doing any real gRPC IO.
struct QueryDataAdapter {
    messages: Box<
        dyn Iterator<Item = Result<(LowLevelMessage, IngesterQueryResponseMetadata), FlightError>>
            + Send,
    >,
}

impl std::fmt::Debug for QueryDataAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryDataAdapter").finish_non_exhaustive()
    }
}

impl QueryDataAdapter {
    /// Create new adapter.
    ///
    /// This pre-calculates some data structure that we are going to need later.
    async fn new(response: IngesterQueryResponse) -> Self {
        let mut messages = vec![];
        let mut stream = response.flatten();
        while let Some(msg_res) = stream.next().await {
            match msg_res {
                Ok(msg) => {
                    let (msg, md) = match msg {
                        FlatIngesterQueryResponse::StartPartition {
                            partition_id,
                            status,
                        } => (
                            LowLevelMessage::None,
                            IngesterQueryResponseMetadata {
                                partition_id: partition_id.get(),
                                status: Some(PartitionStatus {
                                    parquet_max_sequence_number: status
                                        .parquet_max_sequence_number
                                        .map(|x| x.get()),
                                    tombstone_max_sequence_number: status
                                        .tombstone_max_sequence_number
                                        .map(|x| x.get()),
                                }),
                            },
                        ),
                        FlatIngesterQueryResponse::StartSnapshot { schema } => (
                            LowLevelMessage::Schema(schema),
                            IngesterQueryResponseMetadata::default(),
                        ),
                        FlatIngesterQueryResponse::RecordBatch { batch } => (
                            LowLevelMessage::RecordBatch(batch),
                            IngesterQueryResponseMetadata::default(),
                        ),
                    };
                    messages.push(Ok((msg, md)));
                }
                Err(e) => {
                    messages.push(Err(FlightError::ArrowError(e)));
                }
            }
        }

        Self {
            messages: Box::new(messages.into_iter()),
        }
    }
}

#[async_trait]
impl IngesterFlightClientQueryData for QueryDataAdapter {
    async fn next(
        &mut self,
    ) -> Result<Option<(LowLevelMessage, IngesterQueryResponseMetadata)>, FlightError> {
        self.messages.next().transpose()
    }
}
