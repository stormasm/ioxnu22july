use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use data_types::{PartitionTemplate, TemplatePart};
use hyper::{Body, Request};
use iox_catalog::{interface::Catalog, mem::MemCatalog};
use router::{
    dml_handlers::{
        DmlHandlerChainExt, FanOutAdaptor, Partitioner, SchemaValidator, ShardedWriteBuffer,
        WriteSummaryAdapter,
    },
    namespace_cache::{MemoryNamespaceCache, ShardedCache},
    sequencer::Sequencer,
    server::http::HttpDelegate,
};
use sharder::JumpHash;
use std::{collections::BTreeSet, iter, sync::Arc};
use tokio::runtime::Runtime;
use write_buffer::{
    core::WriteBufferWriting,
    mock::{MockBufferForWriting, MockBufferSharedState},
};

// Init a mock write buffer with the given number of sequencers.
fn init_write_buffer(n_sequencers: u32) -> ShardedWriteBuffer<JumpHash<Arc<Sequencer>>> {
    let time = iox_time::MockProvider::new(iox_time::Time::from_timestamp_millis(668563200000));
    let write_buffer: Arc<dyn WriteBufferWriting> = Arc::new(
        MockBufferForWriting::new(
            MockBufferSharedState::empty_with_n_sequencers(
                n_sequencers.try_into().expect("cannot have 0 sequencers"),
            ),
            None,
            Arc::new(time),
        )
        .expect("failed to init mock write buffer"),
    );

    let shards: BTreeSet<_> = write_buffer.sequencer_ids();
    ShardedWriteBuffer::new(
        JumpHash::new(
            shards
                .into_iter()
                .map(|id| Sequencer::new(id as _, Arc::clone(&write_buffer), &Default::default()))
                .map(Arc::new),
        )
        .expect("failed to init sharder"),
    )
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
}

fn e2e_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("e2e");

    let delegate = {
        let metrics = Arc::new(metric::Registry::new());
        let catalog: Arc<dyn Catalog> = Arc::new(MemCatalog::new(Arc::clone(&metrics)));
        let ns_cache = Arc::new(
            ShardedCache::new(
                iter::repeat_with(|| Arc::new(MemoryNamespaceCache::default())).take(10),
            )
            .unwrap(),
        );

        let write_buffer = init_write_buffer(1);
        let schema_validator =
            SchemaValidator::new(Arc::clone(&catalog), Arc::clone(&ns_cache), &*metrics);
        let partitioner = Partitioner::new(PartitionTemplate {
            parts: vec![TemplatePart::TimeFormat("%Y-%m-%d".to_owned())],
        });

        let handler_stack = schema_validator.and_then(
            partitioner.and_then(WriteSummaryAdapter::new(FanOutAdaptor::new(write_buffer))),
        );

        HttpDelegate::new(1024, 100, Arc::new(handler_stack), &metrics)
    };

    let body_str = "platanos,tag1=A,tag2=B val=42i 123456";

    group.throughput(Throughput::Bytes(body_str.len() as u64));
    group.bench_function("e2e", |b| {
        b.to_async(runtime()).iter(|| {
            let request = Request::builder()
                .uri("https://bananas.example/api/v2/write?org=bananas&bucket=test")
                .method("POST")
                .body(Body::from(body_str.as_bytes()))
                .unwrap();
            delegate.route(request)
        })
    });

    group.finish();
}

criterion_group!(benches, e2e_benchmarks);
criterion_main!(benches);
