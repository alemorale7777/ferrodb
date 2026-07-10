//! Micro-benchmark: scalar vs SIMD distance kernels.
//!
//! Run with:
//!   cargo run -p vector --example distbench --release
//!
//! Methodology: many repetitions over a working set larger than L1, sum the
//! results into a sink so the optimizer cannot delete the loop, report
//! ns/call. This is a *kernel* benchmark (hot cache, predictable branches);
//! end-to-end search throughput is measured by the recall harness instead.

use std::hint::black_box;
use std::time::Instant;

use vector::distance::{kernels, Metric, SCALAR};

fn bench(name: &str, dim: usize, f: impl Fn(&[f32], &[f32]) -> f32) {
    // 256 pseudo-random vectors ≈ 1 MB at dim 1024 — larger than L1, fits L2.
    let nvec = 256;
    let mut vecs = vec![0.0f32; nvec * dim];
    let mut seed = 0x2545_F491u32;
    for x in vecs.iter_mut() {
        // xorshift: cheap, deterministic, no rand crate.
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *x = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
    }
    let q = &vecs[..dim];

    // Warm up, then measure.
    let reps = 2000;
    let mut sink = 0.0f32;
    for row in 0..nvec {
        sink += f(q, &vecs[row * dim..(row + 1) * dim]);
    }
    let start = Instant::now();
    for _ in 0..reps {
        for row in 0..nvec {
            sink += f(black_box(q), black_box(&vecs[row * dim..(row + 1) * dim]));
        }
    }
    let elapsed = start.elapsed();
    let calls = (reps * nvec) as f64;
    println!(
        "{name:<28} dim={dim:<5} {:>8.1} ns/call   ({:.2} Gf32/s)  [sink {sink:.1}]",
        elapsed.as_nanos() as f64 / calls,
        (calls * dim as f64) / elapsed.as_secs_f64() / 1e9,
    );
}

fn main() {
    let k = kernels();
    println!(
        "runtime kernel set: {}\n",
        if k.simd {
            "AVX2+FMA"
        } else {
            "scalar (no AVX2 detected)"
        }
    );
    for dim in [128usize, 768, 1536] {
        bench("dot  scalar", dim, SCALAR.dot);
        bench("dot  runtime", dim, k.dot);
        bench("l2   scalar", dim, SCALAR.l2_sq);
        bench("l2   runtime", dim, k.l2_sq);
        bench("cos  scalar", dim, |a, b| {
            SCALAR.distance(Metric::Cosine, a, b)
        });
        bench("cos  runtime", dim, move |a, b| {
            k.distance(Metric::Cosine, a, b)
        });
        println!();
    }
}
