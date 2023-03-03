use std::fmt::{Display, Formatter};
use std::io::Error;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use async_trait::async_trait;
use data_types::SequenceNumber;
use dml::{DmlMeta, DmlOperation, DmlWrite};
use futures::stream::BoxStream;

/// Generic boxed error type that is used in this crate.
///
/// The dynamic boxing makes it easier to deal with error from different implementations.
#[derive(Debug)]
pub struct WriteBufferError {
    inner: Box<dyn std::error::Error + Sync + Send>,
    kind: WriteBufferErrorKind,
}

impl WriteBufferError {
    pub fn new(
        kind: WriteBufferErrorKind,
        e: impl Into<Box<dyn std::error::Error + Sync + Send>>,
    ) -> Self {
        Self {
            inner: e.into(),
            kind,
        }
    }

    pub fn invalid_data(e: impl Into<Box<dyn std::error::Error + Sync + Send>>) -> Self {
        Self::new(WriteBufferErrorKind::InvalidData, e)
    }

    pub fn invalid_input(e: impl Into<Box<dyn std::error::Error + Sync + Send>>) -> Self {
        Self::new(WriteBufferErrorKind::InvalidInput, e)
    }

    pub fn unknown_sequence_number(e: impl Into<Box<dyn std::error::Error + Sync + Send>>) -> Self {
        Self::new(WriteBufferErrorKind::UnknownSequenceNumber, e)
    }

    pub fn unknown(e: impl Into<Box<dyn std::error::Error + Sync + Send>>) -> Self {
        Self::new(WriteBufferErrorKind::Unknown, e)
    }

    /// Returns the kind of error this was
    pub fn kind(&self) -> WriteBufferErrorKind {
        self.kind
    }

    /// Returns the inner error
    pub fn inner(&self) -> &dyn std::error::Error {
        self.inner.as_ref()
    }
}

impl Display for WriteBufferError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteBufferError({:?}): {}", self.kind, self.inner)
    }
}

impl std::error::Error for WriteBufferError {}

impl From<std::io::Error> for WriteBufferError {
    fn from(e: Error) -> Self {
        Self {
            inner: Box::new(e),
            kind: WriteBufferErrorKind::IO,
        }
    }
}

impl From<rskafka::client::error::Error> for WriteBufferError {
    fn from(e: rskafka::client::error::Error) -> Self {
        Self {
            inner: Box::new(e),
            kind: WriteBufferErrorKind::IO,
        }
    }
}

impl From<rskafka::client::producer::Error> for WriteBufferError {
    fn from(e: rskafka::client::producer::Error) -> Self {
        Self {
            inner: Box::new(e),
            kind: WriteBufferErrorKind::IO,
        }
    }
}

impl From<String> for WriteBufferError {
    fn from(e: String) -> Self {
        Self {
            inner: e.into(),
            kind: WriteBufferErrorKind::Unknown,
        }
    }
}

impl From<&'static str> for WriteBufferError {
    fn from(e: &'static str) -> Self {
        Self {
            inner: e.into(),
            kind: WriteBufferErrorKind::Unknown,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WriteBufferErrorKind {
    /// This operation failed for an unknown reason
    Unknown,

    /// This operation was provided with invalid input data
    InvalidInput,

    /// This operation encountered invalid data
    InvalidData,

    /// A fatal IO error occurred - non-fatal errors should be retried internally
    IO,

    /// The sequence number that we are trying to read is unknown.
    UnknownSequenceNumber,
}

/// Writing to a Write Buffer takes a [`DmlWrite`] and returns the [`DmlMeta`] for the
/// payload that was written
#[async_trait]
pub trait WriteBufferWriting: Sync + Send + Debug + 'static {
    /// List all known sequencers.
    ///
    /// This set not empty.
    fn sequencer_ids(&self) -> BTreeSet<u32>;

    /// Send a [`DmlOperation`] to the write buffer using the specified sequencer ID.
    ///
    /// The [`dml::DmlMeta`] will be propagated where applicable
    ///
    /// This call may "async block" (i.e. be in a pending state) to accumulate multiple operations
    /// into a single batch. After this method returns the operation was actually written (i.e. it
    /// is NOT buffered any longer). You may use [`flush`](Self::flush) to trigger an early
    /// submission (e.g. before some linger time expired), which can be helpful for controlled
    /// shutdown.
    ///
    /// Returns the metadata that was written.
    async fn store_operation(
        &self,
        sequencer_id: u32,
        operation: DmlOperation,
    ) -> Result<DmlMeta, WriteBufferError>;

    /// Sends line protocol to the write buffer - primarily intended for testing
    async fn store_lp(
        &self,
        sequencer_id: u32,
        lp: &str,
        default_time: i64,
    ) -> Result<DmlMeta, WriteBufferError> {
        let tables = mutable_batch_lp::lines_to_batches(lp, default_time)
            .map_err(WriteBufferError::invalid_input)?;

        self.store_operation(
            sequencer_id,
            DmlOperation::Write(DmlWrite::new("test_db", tables, None, Default::default())),
        )
        .await
    }

    /// Flush all currently blocking store operations ([`store_operation`](Self::store_operation) /
    /// [`store_lp`](Self::store_lp)).
    ///
    /// This call is pending while outstanding data is being submitted and will return AFTER the
    /// flush completed. However you still need to poll the store operations to get the metadata
    /// for every write.
    async fn flush(&self) -> Result<(), WriteBufferError>;

    /// Return type (like `"mock"` or `"kafka"`) of this writer.
    fn type_name(&self) -> &'static str;
}

/// Handles a stream of a specific sequencer.
///
/// This can be used to consume data via a stream or to seek the stream to a given sequence number.
#[async_trait]
pub trait WriteBufferStreamHandler: Sync + Send + Debug + 'static {
    /// Stream that produces DML operations.
    ///
    /// Note that due to the mutable borrow, it is not possible to have multiple streams from the
    /// same [`WriteBufferStreamHandler`] instance at the same time. If all streams are dropped and
    /// requested again, the last sequence number of the old streams will be the start sequence
    /// number for the new streams. If you want to prevent that either create a new
    /// [`WriteBufferStreamHandler`] or use [`seek`](Self::seek).
    ///
    /// If the sequence number that the stream wants to read is unknown (either because it is in
    /// the future or because some retention policy removed it already), the stream will return an
    /// error with [`WriteBufferErrorKind::UnknownSequenceNumber`] and will end immediately.
    async fn stream(&mut self) -> BoxStream<'static, Result<DmlOperation, WriteBufferError>>;

    /// Seek sequencer to given sequence number. The next output of related streams will be an
    /// entry with at least the given sequence number (the actual sequence number might be skipped
    /// due to "holes" in the stream).
    ///
    /// Note that due to the mutable borrow, it is not possible to seek while streams exists.
    async fn seek(&mut self, sequence_number: SequenceNumber) -> Result<(), WriteBufferError>;

    /// Reset the sequencer to whatever is the earliest number available in the retained write
    /// buffer. Useful to restart if [`WriteBufferErrorKind::UnknownSequenceNumber`] is returned
    /// from [`stream`](Self::stream) but that isn't a problem.
    fn reset_to_earliest(&mut self);
}

#[async_trait]
impl WriteBufferStreamHandler for Box<dyn WriteBufferStreamHandler> {
    async fn stream(&mut self) -> BoxStream<'static, Result<DmlOperation, WriteBufferError>> {
        self.as_mut().stream().await
    }

    async fn seek(&mut self, sequence_number: SequenceNumber) -> Result<(), WriteBufferError> {
        self.as_mut().seek(sequence_number).await
    }

    fn reset_to_earliest(&mut self) {
        self.as_mut().reset_to_earliest()
    }
}

/// Produce streams (one per sequencer) of [`DmlWrite`]s.
#[async_trait]
pub trait WriteBufferReading: Sync + Send + Debug + 'static {
    /// List all known sequencers.
    ///
    /// This set not empty.
    fn sequencer_ids(&self) -> BTreeSet<u32>;

    /// Get stream handler for a dedicated sequencer.
    ///
    /// Handlers do NOT share any state (e.g. last sequence number).
    async fn stream_handler(
        &self,
        sequencer_id: u32,
    ) -> Result<Box<dyn WriteBufferStreamHandler>, WriteBufferError>;

    /// Get stream handlers for all stream.
    async fn stream_handlers(
        &self,
    ) -> Result<BTreeMap<u32, Box<dyn WriteBufferStreamHandler>>, WriteBufferError> {
        let mut handlers = BTreeMap::new();

        for sequencer_id in self.sequencer_ids() {
            handlers.insert(sequencer_id, self.stream_handler(sequencer_id).await?);
        }

        Ok(handlers)
    }

    /// Get high watermark (= what we believe is the next sequence number to be added).
    ///
    /// Can be used to calculate lag. Note that since the watermark is "next sequence ID number to
    /// be added", it starts at 0 and after the entry with sequence number 0 is added to the
    /// buffer, it is 1.
    async fn fetch_high_watermark(
        &self,
        sequencer_id: u32,
    ) -> Result<SequenceNumber, WriteBufferError>;

    /// Return type (like `"mock"` or `"kafka"`) of this reader.
    fn type_name(&self) -> &'static str;
}

pub mod test_utils {
    //! Generic tests for all write buffer implementations.
    use crate::core::WriteBufferErrorKind;

    use super::{
        WriteBufferError, WriteBufferReading, WriteBufferStreamHandler, WriteBufferWriting,
    };
    use async_trait::async_trait;
    use data_types::{PartitionKey, SequenceNumber};
    use dml::{test_util::assert_write_op_eq, DmlMeta, DmlOperation, DmlWrite};
    use futures::{stream::FuturesUnordered, Stream, StreamExt, TryStreamExt};
    use iox_time::{Time, TimeProvider};
    use std::{
        collections::{BTreeSet, HashSet},
        convert::TryFrom,
        num::NonZeroU32,
        sync::Arc,
        time::Duration,
    };
    use trace::{ctx::SpanContext, span::Span, RingBufferTraceCollector};
    use uuid::Uuid;

    /// Generated random topic name for testing.
    pub fn random_topic_name() -> String {
        format!("test_topic_{}", Uuid::new_v4())
    }

    /// Adapter to make a concrete write buffer implementation work w/ [`perform_generic_tests`].
    #[async_trait]
    pub trait TestAdapter: Send + Sync {
        /// The context type that is used.
        type Context: TestContext;

        /// Create a new context.
        ///
        /// This will be called multiple times during the test suite. Each resulting context must
        /// represent an isolated environment.
        async fn new_context(&self, n_sequencers: NonZeroU32) -> Self::Context {
            self.new_context_with_time(n_sequencers, Arc::new(iox_time::SystemProvider::new()))
                .await
        }

        async fn new_context_with_time(
            &self,
            n_sequencers: NonZeroU32,
            time_provider: Arc<dyn TimeProvider>,
        ) -> Self::Context;
    }

    /// Context used during testing.
    ///
    /// Represents an isolated environment. Actions like sequencer creations and writes must not
    /// leak across context boundaries.
    #[async_trait]
    pub trait TestContext: Send + Sync {
        /// Write buffer writer implementation specific to this context and adapter.
        type Writing: WriteBufferWriting;

        /// Write buffer reader implementation specific to this context and adapter.
        type Reading: WriteBufferReading;

        /// Create new writer.
        async fn writing(&self, creation_config: bool) -> Result<Self::Writing, WriteBufferError>;

        /// Create new reader.
        async fn reading(&self, creation_config: bool) -> Result<Self::Reading, WriteBufferError>;

        /// Trace collector that is used in this context.
        fn trace_collector(&self) -> Arc<RingBufferTraceCollector>;
    }

    /// Generic test suite that must be passed by all proper write buffer implementations.
    ///
    /// See [`TestAdapter`] for how to make a concrete write buffer implementation work with this
    /// test suite.
    ///
    /// Note that you might need more tests on top of this to assert specific implementation
    /// behaviors, edge cases, and error handling.
    pub async fn perform_generic_tests<T>(adapter: T)
    where
        T: TestAdapter,
    {
        test_single_stream_io(&adapter).await;
        test_multi_stream_io(&adapter).await;
        test_multi_sequencer_io(&adapter).await;
        test_multi_writer_multi_reader(&adapter).await;
        test_seek(&adapter).await;
        test_reset_to_earliest(&adapter).await;
        test_watermark(&adapter).await;
        test_timestamp(&adapter).await;
        test_timestamp_batching(&adapter).await;
        test_sequencer_auto_creation(&adapter).await;
        test_sequencer_ids(&adapter).await;
        test_span_context(&adapter).await;
        test_unknown_sequencer_write(&adapter).await;
        test_multi_namespaces(&adapter).await;
        test_flush(&adapter).await;
    }

    /// Writes line protocol and returns the [`DmlWrite`] that was written
    pub async fn write(
        namespace: &str,
        writer: &impl WriteBufferWriting,
        lp: &str,
        sequencer_id: u32,
        partition_key: PartitionKey,
        span_context: Option<&SpanContext>,
    ) -> DmlWrite {
        let tables = mutable_batch_lp::lines_to_batches(lp, 0).unwrap();
        let write = DmlWrite::new(
            namespace,
            tables,
            Some(partition_key),
            DmlMeta::unsequenced(span_context.cloned()),
        );
        let operation = DmlOperation::Write(write);

        let meta = writer
            .store_operation(sequencer_id, operation.clone())
            .await
            .unwrap();

        let mut write = match operation {
            DmlOperation::Write(write) => write,
            _ => unreachable!(),
        };

        write.set_meta(meta);
        write
    }

    /// Test IO with a single writer and single reader stream.
    ///
    /// This tests that:
    ///
    /// - streams process data in order
    /// - readers can handle the "pending" state w/o erroring
    /// - readers unblock after being in "pending" state
    async fn test_single_stream_io<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let entry_1 = "upc user=1 100";
        let entry_2 = "upc user=2 200";
        let entry_3 = "upc user=3 300";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let sequencer_id = set_pop_first(&mut reader.sequencer_ids()).unwrap();
        let mut stream_handler = reader.stream_handler(sequencer_id).await.unwrap();
        let mut stream = stream_handler.stream().await;

        // empty stream is pending
        assert_stream_pending(&mut stream).await;

        // adding content allows us to get results
        let w1 = write(
            "namespace",
            &writer,
            entry_1,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w1);

        // stream is pending again
        assert_stream_pending(&mut stream).await;

        // adding more data unblocks the stream
        let w2 = write(
            "namespace",
            &writer,
            entry_2,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w3 = write(
            "namespace",
            &writer,
            entry_3,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w2);
        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w3);

        // stream is pending again
        assert_stream_pending(&mut stream).await;
    }

    /// Tests multiple subsequently created streams from a single [`WriteBufferStreamHandler`].
    ///
    /// This tests that:
    ///
    /// - readers remember their sequence number (and "pending" state) even when streams are dropped
    /// - state is not shared between handlers
    async fn test_multi_stream_io<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let entry_1 = "upc user=1 100";
        let entry_2 = "upc user=2 200";
        let entry_3 = "upc user=3 300";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let w1 = write(
            "namespace",
            &writer,
            entry_1,
            0,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w2 = write(
            "namespace",
            &writer,
            entry_2,
            0,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w3 = write(
            "namespace",
            &writer,
            entry_3,
            0,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        // creating stream, drop stream, re-create it => still starts at first entry
        let sequencer_id = set_pop_first(&mut reader.sequencer_ids()).unwrap();
        let mut stream_handler = reader.stream_handler(sequencer_id).await.unwrap();
        let stream = stream_handler.stream();
        drop(stream);
        let mut stream = stream_handler.stream().await;
        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w1);

        // re-creating stream after reading remembers sequence number, but wait a bit to provoke
        // the stream to buffer some entries
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(stream);
        let mut stream = stream_handler.stream().await;
        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w2);
        assert_write_op_eq(&stream.next().await.unwrap().unwrap(), &w3);

        // re-creating stream after reading everything makes it pending
        drop(stream);
        let mut stream = stream_handler.stream().await;
        assert_stream_pending(&mut stream).await;

        // use a different handler => stream starts from beginning
        let mut stream_handler2 = reader.stream_handler(sequencer_id).await.unwrap();
        let mut stream2 = stream_handler2.stream().await;
        assert_write_op_eq(&stream2.next().await.unwrap().unwrap(), &w1);
        assert_stream_pending(&mut stream).await;
    }

    /// Test single reader-writer IO w/ multiple sequencers.
    ///
    /// This tests that:
    ///
    /// - writes go to and reads come from the right sequencer, aka that sequencers provide a
    ///   namespace-like isolation
    /// - "pending" states are specific to a sequencer
    async fn test_multi_sequencer_io<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(2).unwrap()).await;

        let entry_1 = "upc user=1 100";
        let entry_2 = "upc user=2 200";
        let entry_3 = "upc user=3 300";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        // check that we have two different sequencer IDs
        let mut sequencer_ids = reader.sequencer_ids();
        assert_eq!(sequencer_ids.len(), 2);
        let sequencer_id_1 = set_pop_first(&mut sequencer_ids).unwrap();
        let sequencer_id_2 = set_pop_first(&mut sequencer_ids).unwrap();
        assert_ne!(sequencer_id_1, sequencer_id_2);

        let mut stream_handler_1 = reader.stream_handler(sequencer_id_1).await.unwrap();
        let mut stream_handler_2 = reader.stream_handler(sequencer_id_2).await.unwrap();
        let mut stream_1 = stream_handler_1.stream().await;
        let mut stream_2 = stream_handler_2.stream().await;

        // empty streams are pending
        assert_stream_pending(&mut stream_1).await;
        assert_stream_pending(&mut stream_2).await;

        // entries arrive at the right target stream
        let w1 = write(
            "namespace",
            &writer,
            entry_1,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        assert_write_op_eq(&stream_1.next().await.unwrap().unwrap(), &w1);
        assert_stream_pending(&mut stream_2).await;

        let w2 = write(
            "namespace",
            &writer,
            entry_2,
            sequencer_id_2,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        assert_stream_pending(&mut stream_1).await;
        assert_write_op_eq(&stream_2.next().await.unwrap().unwrap(), &w2);

        let w3 = write(
            "namespace",
            &writer,
            entry_3,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        assert_stream_pending(&mut stream_2).await;
        assert_write_op_eq(&stream_1.next().await.unwrap().unwrap(), &w3);

        // streams are pending again
        assert_stream_pending(&mut stream_1).await;
        assert_stream_pending(&mut stream_2).await;
    }

    /// Test multiple multiple writers and multiple readers on multiple sequencers.
    ///
    /// This tests that:
    ///
    /// - writers retrieve consistent sequencer IDs
    /// - writes go to and reads come from the right sequencer, similar
    ///   to [`test_multi_sequencer_io`] but less detailed
    /// - multiple writers can write to a single sequencer
    async fn test_multi_writer_multi_reader<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(2).unwrap()).await;

        let entry_east_1 = "upc,region=east user=1 100";
        let entry_east_2 = "upc,region=east user=2 200";
        let entry_west_1 = "upc,region=west user=1 200";

        let writer_1 = context.writing(true).await.unwrap();
        let writer_2 = context.writing(true).await.unwrap();
        let reader_1 = context.reading(true).await.unwrap();
        let reader_2 = context.reading(true).await.unwrap();

        let mut sequencer_ids_1 = writer_1.sequencer_ids();
        let sequencer_ids_2 = writer_2.sequencer_ids();
        assert_eq!(sequencer_ids_1, sequencer_ids_2);
        assert_eq!(sequencer_ids_1.len(), 2);
        let sequencer_id_1 = set_pop_first(&mut sequencer_ids_1).unwrap();
        let sequencer_id_2 = set_pop_first(&mut sequencer_ids_1).unwrap();

        let w_east_1 = write(
            "namespace",
            &writer_1,
            entry_east_1,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w_west_1 = write(
            "namespace",
            &writer_1,
            entry_west_1,
            sequencer_id_2,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w_east_2 = write(
            "namespace",
            &writer_2,
            entry_east_2,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        let mut handler_1_1 = reader_1.stream_handler(sequencer_id_1).await.unwrap();
        let mut handler_1_2 = reader_1.stream_handler(sequencer_id_2).await.unwrap();
        let mut handler_2_1 = reader_2.stream_handler(sequencer_id_1).await.unwrap();
        let mut handler_2_2 = reader_2.stream_handler(sequencer_id_2).await.unwrap();

        assert_reader_content(&mut handler_1_1, &[&w_east_1, &w_east_2]).await;
        assert_reader_content(&mut handler_1_2, &[&w_west_1]).await;
        assert_reader_content(&mut handler_2_1, &[&w_east_1, &w_east_2]).await;
        assert_reader_content(&mut handler_2_2, &[&w_west_1]).await;
    }

    /// Test seek implemention of readers.
    ///
    /// This tests that:
    ///
    /// - seeking is specific to the reader AND sequencer
    /// - forward and backwards seeking works
    /// - seeking past the end of the known content works (results in "pending" status and
    ///   remembers sequence number and not just "next entry")
    async fn test_seek<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(2).unwrap()).await;

        let entry_east_1 = "upc,region=east user=1 100";
        let entry_east_2 = "upc,region=east user=2 200";
        let entry_east_3 = "upc,region=east user=3 300";
        let entry_west_1 = "upc,region=west user=1 200";

        let writer = context.writing(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        let sequencer_id_1 = set_pop_first(&mut sequencer_ids).unwrap();
        let sequencer_id_2 = set_pop_first(&mut sequencer_ids).unwrap();

        let w_east_1 = write(
            "namespace",
            &writer,
            entry_east_1,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w_east_2 = write(
            "namespace",
            &writer,
            entry_east_2,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w_west_1 = write(
            "namespace",
            &writer,
            entry_west_1,
            sequencer_id_2,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        let reader_1 = context.reading(true).await.unwrap();
        let reader_2 = context.reading(true).await.unwrap();

        let mut handler_1_1_a = reader_1.stream_handler(sequencer_id_1).await.unwrap();
        let mut handler_1_2_a = reader_1.stream_handler(sequencer_id_2).await.unwrap();
        let mut handler_1_1_b = reader_1.stream_handler(sequencer_id_1).await.unwrap();
        let mut handler_1_2_b = reader_1.stream_handler(sequencer_id_2).await.unwrap();
        let mut handler_2_1 = reader_2.stream_handler(sequencer_id_1).await.unwrap();
        let mut handler_2_2 = reader_2.stream_handler(sequencer_id_2).await.unwrap();

        // forward seek
        handler_1_1_a
            .seek(w_east_2.meta().sequence().unwrap().sequence_number)
            .await
            .unwrap();

        assert_reader_content(&mut handler_1_1_a, &[&w_east_2]).await;
        assert_reader_content(&mut handler_1_2_a, &[&w_west_1]).await;
        assert_reader_content(&mut handler_1_1_b, &[&w_east_1, &w_east_2]).await;
        assert_reader_content(&mut handler_1_2_b, &[&w_west_1]).await;
        assert_reader_content(&mut handler_2_1, &[&w_east_1, &w_east_2]).await;
        assert_reader_content(&mut handler_2_2, &[&w_west_1]).await;

        // backward seek
        handler_1_1_a.seek(SequenceNumber::new(0)).await.unwrap();
        assert_reader_content(&mut handler_1_1_a, &[&w_east_1, &w_east_2]).await;

        // seek to far end and then add data
        // The affected stream should error and then stop. The other streams should still be
        // pending.
        handler_1_1_a
            .seek(SequenceNumber::new(1_000_000))
            .await
            .unwrap();
        let w_east_3 = write(
            "namespace",
            &writer,
            entry_east_3,
            0,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        let err = handler_1_1_a
            .stream()
            .await
            .next()
            .await
            .expect("stream not ended")
            .unwrap_err();
        assert_eq!(err.kind(), WriteBufferErrorKind::UnknownSequenceNumber);
        assert!(handler_1_1_a.stream().await.next().await.is_none());

        assert_stream_pending(&mut handler_1_2_a.stream().await).await;
        assert_reader_content(&mut handler_1_1_b, &[&w_east_3]).await;
        assert_stream_pending(&mut handler_1_2_b.stream().await).await;
        assert_reader_content(&mut handler_2_1, &[&w_east_3]).await;
        assert_stream_pending(&mut handler_2_2.stream().await).await;

        // seeking again should recover the stream
        handler_1_1_a.seek(SequenceNumber::new(0)).await.unwrap();
        assert_reader_content(&mut handler_1_1_a, &[&w_east_1, &w_east_2, &w_east_3]).await;
    }

    /// Test reset to earliest implemention of readers.
    ///
    /// This tests that:
    ///
    /// - Calling the function jumps to the earliest available sequence number if the earliest
    ///   available sequence number is earlier than the current sequence number
    /// - Calling the function jumps to the earliest available sequence number if the earliest
    ///   available sequence number is later than the current sequence number
    async fn test_reset_to_earliest<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(2).unwrap()).await;

        let entry_east_1 = "upc,region=east user=1 100";
        let entry_east_2 = "upc,region=east user=2 200";

        let writer = context.writing(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        let sequencer_id_1 = set_pop_first(&mut sequencer_ids).unwrap();

        let w_east_1 = write(
            "namespace",
            &writer,
            entry_east_1,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w_east_2 = write(
            "namespace",
            &writer,
            entry_east_2,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        let reader_1 = context.reading(true).await.unwrap();

        let mut handler_1_1_a = reader_1.stream_handler(sequencer_id_1).await.unwrap();

        // forward seek
        handler_1_1_a
            .seek(w_east_2.meta().sequence().unwrap().sequence_number)
            .await
            .unwrap();
        assert_reader_content(&mut handler_1_1_a, &[&w_east_2]).await;

        // reset to earliest goes back to 0; stream re-fetches earliest record
        handler_1_1_a.reset_to_earliest();
        assert_reader_content(&mut handler_1_1_a, &[&w_east_1, &w_east_2]).await;

        // TODO: https://github.com/influxdata/influxdb_iox/issues/4651
        // Remove first write operation to simulate retention policies evicting some records
        // reset to earliest goes to whatever's available
    }

    /// Test watermark fetching.
    ///
    /// This tests that:
    ///
    /// - watermarks for empty sequencers is 0
    /// - watermarks for non-empty sequencers is "last sequence ID plus 1"
    async fn test_watermark<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(2).unwrap()).await;

        let entry_east_1 = "upc,region=east user=1 100";
        let entry_east_2 = "upc,region=east user=2 200";
        let entry_west_1 = "upc,region=west user=1 200";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        let sequencer_id_1 = set_pop_first(&mut sequencer_ids).unwrap();
        let sequencer_id_2 = set_pop_first(&mut sequencer_ids).unwrap();

        // start at watermark 0
        assert_eq!(
            reader.fetch_high_watermark(sequencer_id_1).await.unwrap(),
            SequenceNumber::new(0),
        );
        assert_eq!(
            reader.fetch_high_watermark(sequencer_id_2).await.unwrap(),
            SequenceNumber::new(0)
        );

        // high water mark moves
        write(
            "namespace",
            &writer,
            entry_east_1,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w1 = write(
            "namespace",
            &writer,
            entry_east_2,
            sequencer_id_1,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w2 = write(
            "namespace",
            &writer,
            entry_west_1,
            sequencer_id_2,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        assert_eq!(
            reader.fetch_high_watermark(sequencer_id_1).await.unwrap(),
            w1.meta().sequence().unwrap().sequence_number + 1
        );

        assert_eq!(
            reader.fetch_high_watermark(sequencer_id_2).await.unwrap(),
            w2.meta().sequence().unwrap().sequence_number + 1
        );
    }

    /// Test that timestamps reported by the readers are sane.
    async fn test_timestamp<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        // Note: Roundtrips are only guaranteed for millisecond-precision
        let t0 = Time::from_timestamp_millis(129);
        let time = Arc::new(iox_time::MockProvider::new(t0));
        let context = adapter
            .new_context_with_time(
                NonZeroU32::try_from(1).unwrap(),
                Arc::<iox_time::MockProvider>::clone(&time),
            )
            .await;

        let entry = "upc user=1 100";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        assert_eq!(sequencer_ids.len(), 1);
        let sequencer_id = set_pop_first(&mut sequencer_ids).unwrap();

        let write = write(
            "namespace",
            &writer,
            entry,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let reported_ts = write.meta().producer_ts().unwrap();

        // advance time
        time.inc(Duration::from_secs(10));

        // check that the timestamp records the ingestion time, not the read time
        let mut handler = reader.stream_handler(sequencer_id).await.unwrap();
        let sequenced_entry = handler.stream().await.next().await.unwrap().unwrap();
        let ts_entry = sequenced_entry.meta().producer_ts().unwrap();
        assert_eq!(ts_entry, t0);
        assert_eq!(reported_ts, t0);
    }

    /// Test that batching multiple messages to the same partition and
    /// sequencer correctly preserves the timestamps
    ///
    /// Coverage of <https://github.com/influxdata/conductor/issues/1000>
    async fn test_timestamp_batching<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        // Note: Roundtrips are only guaranteed for millisecond-precision
        let t0 = Time::from_timestamp_millis(129);
        let time_provider = Arc::new(iox_time::MockProvider::new(t0));
        let context = adapter
            .new_context_with_time(
                NonZeroU32::try_from(1).unwrap(),
                Arc::clone(&time_provider) as _,
            )
            .await;

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let sequencer_id = set_pop_first(&mut writer.sequencer_ids()).unwrap();

        let bananas_key = PartitionKey::from("bananas");
        let platanos_key = PartitionKey::from("platanos");

        // Two ops with the same partition keys, first write at time 100
        time_provider.set(time_provider.inc(Duration::from_millis(100)));
        write(
            "ns1",
            &writer,
            "table foo=1",
            sequencer_id,
            bananas_key.clone(),
            None,
        )
        .await;

        // second write @ time 200
        time_provider.set(time_provider.inc(Duration::from_millis(100)));
        write(
            "ns1",
            &writer,
            "table foo=1",
            sequencer_id,
            bananas_key.clone(),
            None,
        )
        .await;

        // third write @ time 300
        time_provider.set(time_provider.inc(Duration::from_millis(100)));
        write(
            "ns1",
            &writer,
            "table foo=1",
            sequencer_id,
            platanos_key.clone(),
            None,
        )
        .await;
        drop(writer);

        // now at time 400
        time_provider.set(time_provider.inc(Duration::from_millis(100)));

        let mut handler = reader.stream_handler(sequencer_id).await.unwrap();

        let mut stream = handler.stream().await;

        let dml_op = stream.next().await.unwrap().unwrap();
        assert_eq!(partition_key(&dml_op), Some(&bananas_key));
        assert_eq!(
            dml_op
                .meta()
                .duration_since_production(time_provider.as_ref()),
            Some(Duration::from_millis(300))
        );

        let dml_op = stream.next().await.unwrap().unwrap();
        assert_eq!(partition_key(&dml_op), Some(&bananas_key));
        assert_eq!(
            dml_op
                .meta()
                .duration_since_production(time_provider.as_ref()),
            Some(Duration::from_millis(200))
        );

        let dml_op = stream.next().await.unwrap().unwrap();
        assert_eq!(partition_key(&dml_op), Some(&platanos_key));
        assert_eq!(
            dml_op
                .meta()
                .duration_since_production(time_provider.as_ref()),
            Some(Duration::from_millis(100))
        );
    }

    /// Test that sequencer auto-creation works.
    ///
    /// This tests that:
    ///
    /// - both writer and reader cannot be constructed when sequencers are missing
    /// - both writer and reader can be auto-create sequencers
    async fn test_sequencer_auto_creation<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        // fail when sequencers are missing
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;
        context.writing(false).await.unwrap_err();
        context.reading(false).await.unwrap_err();

        // writer can create sequencers
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;
        context.writing(true).await.unwrap();
        context.writing(false).await.unwrap();
        context.reading(false).await.unwrap();

        // reader can create sequencers
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;
        context.reading(true).await.unwrap();
        context.reading(false).await.unwrap();
        context.writing(false).await.unwrap();
    }

    /// Test sequencer IDs reporting of readers and writers.
    ///
    /// This tests that:
    ///
    /// - all sequencers are reported
    async fn test_sequencer_ids<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let n_sequencers = 10;
        let context = adapter
            .new_context(NonZeroU32::try_from(n_sequencers).unwrap())
            .await;

        let writer_1 = context.writing(true).await.unwrap();
        let writer_2 = context.writing(true).await.unwrap();
        let reader_1 = context.reading(true).await.unwrap();
        let reader_2 = context.reading(true).await.unwrap();

        let sequencer_ids_1 = writer_1.sequencer_ids();
        let sequencer_ids_2 = writer_2.sequencer_ids();
        let sequencer_ids_3 = reader_1.sequencer_ids();
        let sequencer_ids_4 = reader_2.sequencer_ids();
        assert_eq!(sequencer_ids_1.len(), n_sequencers as usize);
        assert_eq!(sequencer_ids_1, sequencer_ids_2);
        assert_eq!(sequencer_ids_1, sequencer_ids_3);
        assert_eq!(sequencer_ids_1, sequencer_ids_4);
    }

    /// Test that span contexts are propagated through the system.
    async fn test_span_context<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let entry = "upc user=1 100";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        assert_eq!(sequencer_ids.len(), 1);
        let sequencer_id = set_pop_first(&mut sequencer_ids).unwrap();
        let mut handler = reader.stream_handler(sequencer_id).await.unwrap();
        let mut stream = handler.stream().await;

        // 1: no context
        write(
            "namespace",
            &writer,
            entry,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        // check write 1
        let write_1 = stream.next().await.unwrap().unwrap();
        assert!(write_1.meta().span_context().is_none());

        // no spans emitted yet
        let collector = context.trace_collector();
        assert!(collector.spans().is_empty());

        // 2: some context
        let span_context_1 = SpanContext::new(Arc::clone(&collector) as Arc<_>);
        write(
            "namespace",
            &writer,
            entry,
            sequencer_id,
            PartitionKey::from("bananas"),
            Some(&span_context_1),
        )
        .await;

        // 2: another context
        let span_context_parent = SpanContext::new(Arc::clone(&collector) as Arc<_>);
        let span_context_2 = span_context_parent.child("foo").ctx;
        write(
            "namespace",
            &writer,
            entry,
            sequencer_id,
            PartitionKey::from("bananas"),
            Some(&span_context_2),
        )
        .await;

        // check write 2
        let write_2 = stream.next().await.unwrap().unwrap();
        let actual_context_1 = write_2.meta().span_context().unwrap();
        assert_span_context_eq_or_linked(&span_context_1, actual_context_1, collector.spans());

        // check write 3
        let write_3 = stream.next().await.unwrap().unwrap();
        let actual_context_2 = write_3.meta().span_context().unwrap();
        assert_span_context_eq_or_linked(&span_context_2, actual_context_2, collector.spans());

        // check that links / parents make sense
        assert_span_relations_closed(&collector.spans(), &[span_context_1, span_context_2]);
    }

    /// Test that writing to an unknown sequencer produces an error
    async fn test_unknown_sequencer_write<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let tables = mutable_batch_lp::lines_to_batches("upc user=1 100", 0).unwrap();
        let write = DmlWrite::new("foo", tables, None, Default::default());
        let operation = DmlOperation::Write(write);

        let writer = context.writing(true).await.unwrap();

        // flip bits to get an unknown sequencer
        let sequencer_id = !set_pop_first(&mut writer.sequencer_ids()).unwrap();
        writer
            .store_operation(sequencer_id, operation)
            .await
            .unwrap_err();
    }

    /// Test usage w/ multiple namespaces.
    ///
    /// Tests that:
    ///
    /// - namespace names or propagated correctly from writer to reader
    /// - all namespaces end up in a single stream
    async fn test_multi_namespaces<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let entry_1 = "upc,region=east user=1 100";
        let entry_2 = "upc,region=east user=2 200";

        let writer = context.writing(true).await.unwrap();
        let reader = context.reading(true).await.unwrap();

        let mut sequencer_ids = writer.sequencer_ids();
        assert_eq!(sequencer_ids.len(), 1);
        let sequencer_id = set_pop_first(&mut sequencer_ids).unwrap();

        let w1 = write(
            "namespace_1",
            &writer,
            entry_2,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;
        let w2 = write(
            "namespace_2",
            &writer,
            entry_1,
            sequencer_id,
            PartitionKey::from("bananas"),
            None,
        )
        .await;

        let mut handler = reader.stream_handler(sequencer_id).await.unwrap();
        assert_reader_content(&mut handler, &[&w1, &w2]).await;
    }

    /// Dummy test to ensure that flushing somewhat works.
    async fn test_flush<T>(adapter: &T)
    where
        T: TestAdapter,
    {
        let context = adapter.new_context(NonZeroU32::try_from(1).unwrap()).await;

        let writer = Arc::new(context.writing(true).await.unwrap());

        let mut sequencer_ids = writer.sequencer_ids();
        assert_eq!(sequencer_ids.len(), 1);
        let sequencer_id = set_pop_first(&mut sequencer_ids).unwrap();

        let mut write_tasks: FuturesUnordered<_> = (0..20)
            .map(|i| {
                let writer = Arc::clone(&writer);

                async move {
                    let entry = format!("upc,region=east user={} {}", i, i);

                    write(
                        "ns",
                        writer.as_ref(),
                        &entry,
                        sequencer_id,
                        PartitionKey::from("bananas"),
                        None,
                    )
                    .await;
                }
            })
            .collect();

        let write_tasks = tokio::spawn(async move { while write_tasks.next().await.is_some() {} });

        tokio::time::sleep(Duration::from_millis(1)).await;

        writer.flush().await.unwrap();

        tokio::time::timeout(Duration::from_millis(1_000), write_tasks)
            .await
            .unwrap()
            .unwrap();
    }

    /// Assert that the content of the reader is as expected.
    ///
    /// This will read `expected_writes.len()` from the reader and then ensures that the stream is
    /// pending.
    async fn assert_reader_content(
        actual_stream_handler: &mut Box<dyn WriteBufferStreamHandler>,
        expected_writes: &[&DmlWrite],
    ) {
        let actual_stream = actual_stream_handler.stream().await;

        // we need to limit the stream to `expected_writes.len()` elements, otherwise it might be
        // pending forever
        let actual_writes: Vec<_> = actual_stream
            .take(expected_writes.len())
            .try_collect()
            .await
            .unwrap();

        assert_eq!(actual_writes.len(), expected_writes.len());
        for (actual, expected) in actual_writes.iter().zip(expected_writes.iter()) {
            assert_write_op_eq(actual, expected);
        }

        // Ensure that stream is pending
        let mut actual_stream = actual_stream_handler.stream().await;
        assert_stream_pending(&mut actual_stream).await;
    }

    /// Asserts that given span context are the same or that `second` links back to `first`.
    ///
    /// "Same" means:
    ///
    /// - identical trace ID
    /// - identical span ID
    /// - identical parent span ID
    pub(crate) fn assert_span_context_eq_or_linked(
        first: &SpanContext,
        second: &SpanContext,
        spans: Vec<Span>,
    ) {
        // search for links
        for span in spans {
            if (span.ctx.trace_id == second.trace_id) && (span.ctx.span_id == second.span_id) {
                // second context was emitted as span

                // check if it links to first context
                for (trace_id, span_id) in span.ctx.links {
                    if (trace_id == first.trace_id) && (span_id == first.span_id) {
                        return;
                    }
                }
            }
        }

        // no link found
        assert_eq!(first.trace_id, second.trace_id);
        assert_eq!(first.span_id, second.span_id);
        assert_eq!(first.parent_span_id, second.parent_span_id);
    }

    /// Assert that all span relations (parents, links) are found within the set of spans or within
    /// the set of roots.
    fn assert_span_relations_closed(spans: &[Span], roots: &[SpanContext]) {
        let all_ids: HashSet<_> = spans
            .iter()
            .map(|span| (span.ctx.trace_id, span.ctx.span_id))
            .chain(roots.iter().map(|ctx| (ctx.trace_id, ctx.span_id)))
            .collect();

        for span in spans {
            if let Some(parent_span_id) = span.ctx.parent_span_id {
                assert!(all_ids.contains(&(span.ctx.trace_id, parent_span_id)));
            }
            for link in &span.ctx.links {
                assert!(all_ids.contains(link));
            }
        }
    }

    /// Assert that given stream is pending.
    ///
    /// This will will try to poll the stream for a bit to ensure that async IO has a chance to
    /// catch up.
    async fn assert_stream_pending<S>(stream: &mut S)
    where
        S: Stream + Send + Unpin,
        S::Item: std::fmt::Debug,
    {
        tokio::select! {
            e = stream.next() => panic!("stream is not pending, yielded: {e:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        };
    }

    /// Pops first entry from set.
    ///
    /// Helper until <https://github.com/rust-lang/rust/issues/62924> is stable.
    pub(crate) fn set_pop_first<T>(set: &mut BTreeSet<T>) -> Option<T>
    where
        T: Clone + Ord,
    {
        set.iter().next().cloned().and_then(|k| set.take(&k))
    }

    /// Get the testing Kafka connection string or return current scope.
    ///
    /// If `TEST_INTEGRATION` and `KAFKA_CONNECT` are set, return the Kafka connection URL to the
    /// caller.
    ///
    /// If `TEST_INTEGRATION` is set but `KAFKA_CONNECT` is not set, fail the tests and provide
    /// guidance for setting `KAFKA_CONNECTION`.
    ///
    /// If `TEST_INTEGRATION` is not set, skip the calling test by returning early.
    #[macro_export]
    macro_rules! maybe_skip_kafka_integration {
        () => {
            maybe_skip_kafka_integration!("")
        };
        ($panic_msg:expr) => {{
            use std::env;
            dotenvy::dotenv().ok();

            let panic_msg: &'static str = $panic_msg;

            match (
                env::var("TEST_INTEGRATION").is_ok(),
                env::var("KAFKA_CONNECT").ok(),
            ) {
                (true, Some(kafka_connection)) => kafka_connection,
                (true, None) => {
                    panic!(
                        "TEST_INTEGRATION is set which requires running integration tests, but \
                        KAFKA_CONNECT is not set. Please run Kafka, perhaps by using the command \
                        `docker-compose -f docker/ci-kafka-docker-compose.yml up kafka`, then \
                        set KAFKA_CONNECT to the host and port where Kafka is accessible. If \
                        running the `docker-compose` command and the Rust tests on the host, the \
                        value for `KAFKA_CONNECT` should be `localhost:9093`. If running the Rust \
                        tests in another container in the `docker-compose` network as on CI, \
                        `KAFKA_CONNECT` should be `kafka:9092`."
                    )
                }
                (false, Some(_)) => {
                    eprintln!("skipping Kafka integration tests - set TEST_INTEGRATION to run");
                    if !panic_msg.is_empty() {
                        panic!("{}", panic_msg);
                    } else {
                        return;
                    }
                }
                (false, None) => {
                    eprintln!(
                        "skipping Kafka integration tests - set TEST_INTEGRATION and KAFKA_CONNECT \
                        to run"
                    );
                    if !panic_msg.is_empty() {
                        panic!("{}", panic_msg);
                    } else {
                        return;
                    }
                }
            }
        }};
    }

    fn partition_key(dml_op: &DmlOperation) -> Option<&PartitionKey> {
        match dml_op {
            DmlOperation::Write(w) => w.partition_key(),
            DmlOperation::Delete(_) => None,
        }
    }
}
