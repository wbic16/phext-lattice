/// Benchmarks for lattice-core operations against the real 4.26 MB phext.
///
/// Run: cargo bench -p lattice-core
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use phext_lattice::{MappedLattice, LatticeIndex, Navigator, search_lattice};
use libphext::phext::to_coordinate;

const PHEXT_PATH: &str = "/source/human/choose-your-own-adventure.phext";

fn bench_index_build(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let buf = lattice.to_phext_bytes();

    c.bench_function("index_build_808_scrolls", |b| {
        b.iter(|| {
            let idx = LatticeIndex::build(black_box(&buf));
            black_box(idx.scroll_count());
        });
    });
}

fn bench_coordinate_lookup(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let buf = lattice.to_phext_bytes();
    let idx = LatticeIndex::build(&buf);
    let coord = to_coordinate("1.1.1/1.1.1/1.1.1");

    c.bench_function("coordinate_lookup", |b| {
        b.iter(|| {
            black_box(idx.get(black_box(&coord)));
        });
    });
}

fn bench_navigate_all(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let idx = lattice.index().clone();

    c.bench_function("navigate_all_808_scrolls", |b| {
        b.iter(|| {
            let mut nav = Navigator::new();
            let mut count = 0u32;
            while nav.next_populated(&idx) {
                count += 1;
            }
            black_box(count);
        });
    });
}

fn bench_search_serial(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let buf = lattice.to_phext_bytes();
    let idx = LatticeIndex::build(&buf);

    c.bench_function("search_serial_common_word", |b| {
        b.iter(|| {
            let hits = search_lattice(
                black_box(&buf),
                &idx,
                black_box("the"),
                false,
                1000,
            );
            black_box(hits.len());
        });
    });

    c.bench_function("search_serial_rare_word", |b| {
        b.iter(|| {
            let hits = search_lattice(
                black_box(&buf),
                &idx,
                black_box("exocortex"),
                false,
                1000,
            );
            black_box(hits.len());
        });
    });
}

fn bench_search_parallel(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let buf = lattice.to_phext_bytes();
    let idx = LatticeIndex::build(&buf);

    c.bench_function("search_parallel_common_word", |b| {
        b.iter(|| {
            let hits = phext_lattice::search::search_lattice_parallel(
                black_box(&buf),
                &idx,
                black_box("the"),
                false,
                1000,
            );
            black_box(hits.len());
        });
    });

    c.bench_function("search_parallel_rare_word", |b| {
        b.iter(|| {
            let hits = phext_lattice::search::search_lattice_parallel(
                black_box(&buf),
                &idx,
                black_box("exocortex"),
                false,
                1000,
            );
            black_box(hits.len());
        });
    });
}

fn bench_sentron_build(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();
    let coord = to_coordinate("1.1.1/1.1.1/1.1.1");

    c.bench_function("sentron_build_808_scrolls", |b| {
        b.iter(|| {
            let sentron = phext_lattice::Sentron::build(black_box(&coord), lattice.index());
            black_box(sentron.size());
        });
    });
}

fn bench_roundtrip(c: &mut Criterion) {
    let lattice = MappedLattice::open(PHEXT_PATH).unwrap();

    c.bench_function("roundtrip_4mb", |b| {
        b.iter(|| {
            let bytes = lattice.to_phext_bytes();
            black_box(bytes.len());
        });
    });
}

criterion_group!(
    benches,
    bench_index_build,
    bench_coordinate_lookup,
    bench_navigate_all,
    bench_search_serial,
    bench_search_parallel,
    bench_sentron_build,
    bench_roundtrip,
);
criterion_main!(benches);
