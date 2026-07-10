//! Recall harness: the HNSW index's correctness proof.
//!
//! HNSW is *approximate* — without a measured recall number the
//! implementation is unverified, no matter how many unit tests pass. This
//! binary builds an index over a labeled dataset, computes exact brute-force
//! ground truth, and reports recall@10 / QPS / latency across an `ef_search`
//! sweep, plus the memory footprint.
//!
//! Dataset: **clustered synthetic Gaussians** (deliberately labeled as such).
//! Uniform random vectors are a misleadingly easy benchmark — real embedding
//! spaces are clustered, and clusters are exactly what stresses the neighbor
//! heuristic. Centers are drawn uniformly in the unit cube; points get
//! Gaussian noise around a random center; queries are fresh draws from the
//! same process (so they are near data but never *in* it).
//!
//! Run: `cargo run -p vector --bin recall --release [-- n dim nq]`

use std::time::Instant;

use vector::distance::Metric;
use vector::hnsw::{Hnsw, HnswParams};

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / (1u64 << 24) as f32
    }
    /// Standard normal via Box–Muller.
    fn gaussian(&mut self) -> f32 {
        let u1 = self.unit_f32().max(1e-7);
        let u2 = self.unit_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

fn make_dataset(rng: &mut Rng, n: usize, dim: usize, ncenters: usize, sigma: f32) -> Vec<f32> {
    let centers: Vec<f32> = (0..ncenters * dim).map(|_| rng.unit_f32()).collect();
    let mut data = Vec::with_capacity(n * dim);
    for _ in 0..n {
        let c = (rng.next_u64() as usize) % ncenters;
        for d in 0..dim {
            data.push(centers[c * dim + d] + sigma * rng.gaussian());
        }
    }
    data
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let n = args.first().copied().unwrap_or(20_000);
    let dim = args.get(1).copied().unwrap_or(64);
    let nq = args.get(2).copied().unwrap_or(200);
    // Cluster tightness (thousandths). sigma≈0.15 resembles real embedding
    // spread; sigma≈0.05 is the near-duplicate stress regime (see report).
    let sigma = args.get(3).copied().unwrap_or(150) as f32 / 1000.0;
    let ncenters = args.get(4).copied().unwrap_or(50);
    let k = 10;
    let params = HnswParams::default();

    println!("dataset: clustered synthetic Gaussians (SYNTHETIC — labeled as such)");
    println!("n={n}  dim={dim}  queries={nq}  k={k}  metric=L2  centers={ncenters}  sigma={sigma}");
    println!(
        "build params: M={}  Mmax0={}  ef_construction={}\n",
        params.m, params.m_max0, params.ef_construction
    );

    let mut rng = Rng(0xFE44_0DB0_1234_5678);
    let data = make_dataset(&mut rng, n, dim, ncenters, sigma);
    let queries = make_dataset(&mut rng, nq, dim, ncenters, sigma);

    // Build.
    let t0 = Instant::now();
    let mut index = Hnsw::new(dim, Metric::L2, params, 42);
    for i in 0..n {
        index.insert(&data[i * dim..(i + 1) * dim], &(i as u64).to_be_bytes());
    }
    let build = t0.elapsed();
    println!(
        "build: {:.2}s  ({:.0} inserts/s)   memory: {:.1} MB",
        build.as_secs_f64(),
        n as f64 / build.as_secs_f64(),
        index.memory_bytes() as f64 / 1e6
    );
    let reach = index.reachable_from_entry();
    println!(
        "reachability: {reach}/{n} nodes reachable at layer 0{}",
        if reach < n {
            "  <-- RECALL CEILING"
        } else {
            ""
        }
    );

    // Exact ground truth (the O(n·dim) scan the index exists to avoid).
    // Hits are counted by DISTANCE, not id: datasets contain ties and
    // near-duplicates, and an id-based count marks "returned a point at the
    // exact same distance under a different id" as a miss — punishing the
    // index for the dataset's ambiguity (ann-benchmarks counts the same way).
    let t0 = Instant::now();
    let truth: Vec<Vec<f32>> = (0..nq)
        .map(|qi| {
            index
                .exact_search(&queries[qi * dim..(qi + 1) * dim], k)
                .into_iter()
                .map(|(d, _)| d)
                .collect()
        })
        .collect();
    let brute = t0.elapsed();
    println!(
        "brute force: {:.1} ms/query ({:.0} QPS) — the baseline HNSW must beat\n",
        brute.as_secs_f64() * 1000.0 / nq as f64,
        nq as f64 / brute.as_secs_f64()
    );

    println!(
        "{:>9} {:>10} {:>9} {:>12} {:>12} {:>10}",
        "ef_search", "recall@10", "recall@1", "mean µs/q", "p95 µs/q", "QPS"
    );
    for ef in [10usize, 20, 40, 80, 160, 320] {
        let mut hits = 0usize;
        let mut hits1 = 0usize;
        let mut lat_us: Vec<f64> = Vec::with_capacity(nq);
        let t0 = Instant::now();
        for qi in 0..nq {
            let q = &queries[qi * dim..(qi + 1) * dim];
            let tq = Instant::now();
            let got = index.search(q, k, ef);
            lat_us.push(tq.elapsed().as_secs_f64() * 1e6);
            let kth = truth[qi][truth[qi].len() - 1];
            let eps = 1e-6 * kth.abs().max(1e-6);
            hits += got.iter().filter(|&&(d, _)| d <= kth + eps).count().min(k);
            if got
                .first()
                .is_some_and(|&(d, _)| d <= truth[qi][0] + 1e-6 * truth[qi][0].abs().max(1e-6))
            {
                hits1 += 1;
            }
        }
        let total = t0.elapsed();
        lat_us.sort_by(|a, b| a.total_cmp(b));
        let recall = hits as f64 / (nq * k) as f64;
        println!(
            "{:>9} {:>10.4} {:>9.4} {:>12.1} {:>12.1} {:>10.0}",
            ef,
            recall,
            hits1 as f64 / nq as f64,
            lat_us.iter().sum::<f64>() / nq as f64,
            lat_us[(nq * 95 / 100).min(nq - 1)],
            nq as f64 / total.as_secs_f64()
        );
    }
    println!("\nreading the table: ef_search buys recall with latency — the");
    println!("single knob a user tunes. recall@10 = fraction of the true 10");
    println!("nearest neighbors the index actually returned.");
}
