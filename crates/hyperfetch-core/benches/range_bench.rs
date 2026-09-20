use criterion::{black_box, criterion_group, criterion_main, Criterion};
use hyperfetch_core::range::ByteRange;
use hyperfetch_core::chunk::ChunkManager;

fn bench_range_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("range_arithmetic");

    let r1 = ByteRange::new(1000, 50000).unwrap();
    let r2 = ByteRange::new(25000, 75000).unwrap();

    group.bench_function("intersection", |b| {
        b.iter(|| black_box(r1).intersection(black_box(&r2)))
    });

    group.bench_function("split_midpoint", |b| {
        b.iter(|| black_box(r1).split_midpoint().unwrap())
    });

    group.finish();
}

fn bench_chunk_manager_work_stealing(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_manager");

    // 1000 chunks of 4MB each (4GB total)
    let total_size = 4 * 1024 * 1024 * 1000;
    let chunk_size = 4 * 1024 * 1024;

    group.bench_function("work_stealing_lookup", |b| {
        b.iter_batched(
            || {
                let mut mgr = ChunkManager::new(total_size, chunk_size).unwrap();
                // Assign first 32 chunks to workers and simulate partial progress
                for worker_id in 0..32 {
                    let _ = mgr.get_next_work(worker_id, 0);
                    let _ = mgr.update_chunk_progress(worker_id, 1024 * 1024, worker_id, 0);
                }
                mgr
            },
            |mut mgr| {
                // Thief attempts to steal work
                black_box(mgr.steal_work(99, 0, 1024 * 1024))
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_range_operations, bench_chunk_manager_work_stealing);
criterion_main!(benches);
