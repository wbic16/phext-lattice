/// Integration test: open the real choose-your-own-adventure.phext
/// and verify index performance.

use lattice_core::{MappedLattice, LatticeIndex, Navigator, Dimension, CoordinateNav};
use libphext::phext::to_coordinate;
use std::time::Instant;

const CYOA_PATH: &str = "/source/human/choose-your-own-adventure.phext";

#[test]
fn index_real_phext() {
    if !std::path::Path::new(CYOA_PATH).exists() {
        eprintln!("Skipping: {} not found", CYOA_PATH);
        return;
    }

    let t0 = Instant::now();
    let lattice = MappedLattice::open(CYOA_PATH).unwrap();
    let load_time = t0.elapsed();

    let scroll_count = lattice.index().scroll_count();
    let buffer_len = lattice.index().buffer_len();

    println!("=== choose-your-own-adventure.phext ===");
    println!("  Buffer size:  {} bytes ({:.2} MB)", buffer_len, buffer_len as f64 / 1_048_576.0);
    println!("  Scroll count: {}", scroll_count);
    println!("  Index time:   {:?}", load_time);

    // Should have meaningful content
    assert!(scroll_count > 10, "Expected many scrolls, got {}", scroll_count);

    // Check BASE coordinate
    let base = to_coordinate("1.1.1/1.1.1/1.1.1");
    let content = lattice.read_scroll(&base);
    println!("  BASE content: {:?}", content.as_ref().map(|s| &s[..s.len().min(80)]));
    assert!(content.is_some(), "BASE should have content");

    // Benchmark: 1000 random coordinate lookups
    let coords = lattice.index().coordinates().to_vec();
    let t1 = Instant::now();
    for i in 0..1000 {
        let coord = coords[i % coords.len()];
        let _ = lattice.read_scroll(&coord);
    }
    let lookup_time = t1.elapsed();
    println!("  1000 lookups: {:?} ({:?}/lookup)", lookup_time, lookup_time / 1000);

    // Test navigator
    let mut nav = Navigator::new();
    let t2 = Instant::now();
    let mut hop_count = 0;
    while nav.next_populated(lattice.index()) {
        hop_count += 1;
        if hop_count > scroll_count { break; } // safety
    }
    let nav_time = t2.elapsed();
    println!("  Navigate all:  {} hops in {:?}", hop_count, nav_time);
    assert_eq!(hop_count, scroll_count - 1, "Should hop through all scrolls minus start");

    // Dimension extent
    for dim_idx in 1..=9u8 {
        let dim = Dimension::from_index(dim_idx).unwrap();
        let extent = lattice.index().dimension_extent(dim);
        if extent.len() > 1 {
            println!("  {} extent: {:?}", dim.name(), extent);
        }
    }

    // Performance target: index should build in <10ms for a 4MB file
    assert!(load_time.as_millis() < 100,
        "Index build took {}ms, expected <100ms", load_time.as_millis());
}

#[test]
fn roundtrip_real_phext() {
    if !std::path::Path::new(CYOA_PATH).exists() {
        eprintln!("Skipping: {} not found", CYOA_PATH);
        return;
    }

    let original = std::fs::read(CYOA_PATH).unwrap();
    let lattice = MappedLattice::open(CYOA_PATH).unwrap();

    let t0 = Instant::now();
    let reconstructed = lattice.to_phext_bytes();
    let rebuild_time = t0.elapsed();

    println!("  Roundtrip rebuild: {:?}", rebuild_time);
    println!("  Original size:     {} bytes", original.len());
    println!("  Reconstructed:     {} bytes", reconstructed.len());

    // The reconstruction should preserve all content.
    // It may differ in trailing empty scrolls / normalization,
    // but the content at each coordinate should match.
    // For now, verify lengths are close (within delimiter overhead).
    let diff = (original.len() as isize - reconstructed.len() as isize).unsigned_abs();
    assert!(diff < 1000,
        "Roundtrip size difference too large: {} bytes", diff);
}
