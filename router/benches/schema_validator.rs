use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BatchSize, BenchmarkGroup, Criterion,
    Throughput,
};
use data_types::DatabaseName;
use hashbrown::HashMap;
use iox_catalog::mem::MemCatalog;
use mutable_batch::MutableBatch;
use once_cell::sync::Lazy;
use router::{
    dml_handlers::{DmlHandler, SchemaValidator},
    namespace_cache::{MemoryNamespaceCache, ShardedCache},
};
use schema::selection::Selection;
use std::{iter, sync::Arc};
use tokio::runtime::Runtime;

static NAMESPACE: Lazy<DatabaseName<'static>> = Lazy::new(|| "bananas".try_into().unwrap());

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

fn sharder_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("schema_validator");

    bench(&mut group, 1, 1);

    bench(&mut group, 1, 100);
    bench(&mut group, 1, 10000);

    bench(&mut group, 100, 1);
    bench(&mut group, 10000, 1);

    group.finish();
}

fn bench(group: &mut BenchmarkGroup<WallTime>, tables: usize, columns_per_table: usize) {
    let metrics = Arc::new(metric::Registry::default());

    let catalog = Arc::new(MemCatalog::new(Arc::clone(&metrics)));
    let ns_cache = Arc::new(
        ShardedCache::new(iter::repeat_with(|| Arc::new(MemoryNamespaceCache::default())).take(10))
            .unwrap(),
    );
    let validator = SchemaValidator::new(catalog, ns_cache, &*metrics);

    for i in 0..65_000 {
        let write = lp_to_writes(format!("{}{}", i + 10_000_000, generate_lp(1, 1)).as_str());
        let _ = runtime().block_on(validator.write(&*NAMESPACE, write, None));
    }

    let write = lp_to_writes(&generate_lp(tables, columns_per_table));
    let column_count = write
        .values()
        .fold(0, |acc, b| acc + b.schema(Selection::All).unwrap().len());

    group.throughput(Throughput::Elements(column_count as _));
    group.bench_function(format!("{tables}x{columns_per_table}"), |b| {
        b.to_async(runtime()).iter_batched(
            || write.clone(),
            |write| validator.write(&*NAMESPACE, write, None),
            BatchSize::SmallInput,
        );
    });
}

fn generate_lp(tables: usize, columns_per_table: usize) -> String {
    (0..tables)
        .map(|i| {
            let cols = (0..columns_per_table)
                .map(|i| format!("val{}=42i", i))
                .collect::<Vec<_>>()
                .join(",");

            format!("table{i},tag=A {cols}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Parse `lp` into a table-keyed MutableBatch map.
fn lp_to_writes(lp: &str) -> HashMap<String, MutableBatch> {
    let (writes, _) = mutable_batch_lp::lines_to_batches_stats(lp, 42)
        .expect("failed to build test writes from LP");
    writes
}

criterion_group!(benches, sharder_benchmarks);
criterion_main!(benches);
