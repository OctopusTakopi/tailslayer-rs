use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tailslayer::{HedgedRuntime, HugePageSize, LayoutPlan, LayoutSpec, ReplicatedBuffer};

fn bench_layout_plan(c: &mut Criterion) {
    let plan = LayoutPlan::for_type::<u8>(LayoutSpec::default()).unwrap();

    c.bench_function("layout_element_offset", |b| {
        let mut index = 0_usize;
        b.iter(|| {
            index = index.wrapping_add(1);
            black_box(plan.element_offset(black_box(index)));
        });
    });
}

fn bench_runtime_read(c: &mut Criterion) {
    let mut buffer = ReplicatedBuffer::<u8>::builder()
        .capacity(64)
        .replicas(2)
        .hugepage_size(HugePageSize::Size2MiB)
        .build()
        .unwrap();
    buffer.extend_from_slice(&[0x43, 0x44, 0x45, 0x46]).unwrap();
    let runtime = HedgedRuntime::builder(buffer).build().unwrap();

    c.bench_function("runtime_read_hugetlb", |b| {
        b.iter_batched(
            || 1_usize,
            |index| {
                black_box(runtime.read(index).unwrap());
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    targets = bench_layout_plan, bench_runtime_read
}
criterion_main!(benches);
