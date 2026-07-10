//! Distance kernels: the innermost loop of every vector search.
//!
//! Three metrics, because embedding models disagree about geometry:
//!
//! - **L2** — squared Euclidean distance. We deliberately skip the final
//!   `sqrt`: it is monotone, and the index only ever *compares* distances, so
//!   ordering by `d²` equals ordering by `d` at a fraction of the cost.
//! - **Cosine** — `1 − cos(a,b)`, an angular distance in `[0, 2]`. Used by
//!   models whose embeddings carry meaning in direction, not magnitude.
//! - **Dot** — negated inner product. For models that pre-normalize their
//!   embeddings, dot product *is* cosine similarity, minus the two norm
//!   computations. Negated so that, like the others, **smaller = closer** —
//!   one uniform convention for the whole index.
//!
//! Each metric has a scalar reference implementation (the specification) and
//! an AVX2+FMA implementation (the speed). Which one runs is decided **once**
//! per process by CPU feature detection — see [`kernels`]. On non-x86_64
//! targets (including WebAssembly) only the scalar path exists.
//!
//! The cosine trick pgvector also plays: [`normalize`] vectors at insert time
//! and cosine collapses to `1 − dot(a, b)` — see [`Metric::Cosine`] docs.

/// How distances are computed. Stored per-index; every vector in an index
/// shares one metric.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    /// Squared Euclidean distance (no `sqrt`; ordering is unchanged).
    L2,
    /// `1 − cosine_similarity`. If all vectors are unit-length (see
    /// [`normalize`]), prefer building the index with [`Metric::Dot`] over
    /// pre-normalized vectors: same ordering, cheaper kernel.
    Cosine,
    /// Negated dot product, so smaller = closer like the other metrics.
    Dot,
}

/// Signature shared by the scalar-result kernels (`dot`, `l2_sq`).
pub type DistanceFn = fn(&[f32], &[f32]) -> f32;
/// Signature of the fused dot+norms kernel backing cosine.
pub type DotNormsFn = fn(&[f32], &[f32]) -> (f32, f32, f32);

/// The resolved kernel set: one function pointer per primitive.
///
/// Resolved once (first use) via [`kernels`]; afterwards every distance call
/// is an indirect call into either the scalar or the AVX2 implementation.
#[derive(Clone, Copy)]
pub struct Kernels {
    /// `Σ aᵢ·bᵢ`
    pub dot: DistanceFn,
    /// `Σ (aᵢ−bᵢ)²`
    pub l2_sq: DistanceFn,
    /// `(Σ aᵢ·bᵢ, Σ aᵢ², Σ bᵢ²)` in a single pass — used by cosine.
    pub dot_norms: DotNormsFn,
    /// True if the AVX2 path was selected (exposed for tests/benchmarks).
    pub simd: bool,
}

impl Kernels {
    /// Distance between `a` and `b` under `metric`. Smaller = closer.
    ///
    /// Panics in debug builds if lengths differ; the index guarantees equal
    /// dimensions by construction (dimension is validated on insert).
    #[inline]
    pub fn distance(&self, metric: Metric, a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len(), "dimension mismatch");
        match metric {
            Metric::L2 => (self.l2_sq)(a, b),
            Metric::Dot => -(self.dot)(a, b),
            Metric::Cosine => {
                let (dot, na, nb) = (self.dot_norms)(a, b);
                let denom = (na * nb).sqrt();
                if denom == 0.0 {
                    // A zero vector has no direction; define it as maximally
                    // distant rather than poisoning the graph with NaN.
                    return 1.0;
                }
                1.0 - dot / denom
            }
        }
    }
}

/// The scalar kernel set (always available; the reference implementation).
pub const SCALAR: Kernels = Kernels {
    dot: dot_scalar,
    l2_sq: l2_sq_scalar,
    dot_norms: dot_norms_scalar,
    simd: false,
};

/// Resolve the best kernel set for this CPU. Detection runs once; the result
/// is a pair of plain function pointers, so the hot loop pays only an
/// indirect call — no per-call feature checks.
#[cfg(target_arch = "x86_64")]
pub fn kernels() -> Kernels {
    use std::sync::OnceLock;
    static CHOICE: OnceLock<Kernels> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            Kernels {
                dot: avx2::dot,
                l2_sq: avx2::l2_sq,
                dot_norms: avx2::dot_norms,
                simd: true,
            }
        } else {
            SCALAR
        }
    })
}

/// On non-x86_64 targets (including wasm32) only the scalar kernels exist.
#[cfg(not(target_arch = "x86_64"))]
pub fn kernels() -> Kernels {
    SCALAR
}

/// Scale `v` to unit length (no-op for the zero vector). Doing this once at
/// insert time turns every query-time cosine into a plain dot product — the
/// same optimization pgvector applies for `vector_cosine_ops`.
pub fn normalize(v: &mut [f32]) {
    let norm = dot_scalar(v, v).sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ---- scalar reference kernels ----------------------------------------------
//
// These are the specification: simple enough to verify by eye, and the ground
// truth the property tests hold the SIMD kernels to.

fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn l2_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

fn dot_norms_scalar(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    (dot, na, nb)
}

// ---- AVX2 + FMA kernels -----------------------------------------------------
//
// 8 f32 lanes per instruction; fused multiply-add halves the instruction count
// and (bonus) rounds once instead of twice. `unsafe` is confined to this
// module: the only obligation is "these instructions exist on this CPU", which
// [`kernels`] proves before ever taking a function pointer from here.
//
// Note the SIMD sums associate differently than the scalar left-to-right sum
// (8 partial accumulators, folded at the end), so results differ from scalar
// by float rounding — that is why the property tests compare within an
// epsilon rather than bit-for-bit.

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    /// Fold the 8 lanes of `v` into one f32.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available.
    #[inline]
    unsafe fn hsum(v: __m256) -> f32 {
        // [a0..a7] -> [a0+a4, a1+a5, a2+a6, a3+a7] -> pairwise -> one lane
        let hi = _mm256_extractf128_ps(v, 1);
        let lo = _mm256_castps256_ps128(v);
        let s = _mm_add_ps(lo, hi);
        let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
        let s = _mm_add_ss(s, _mm_shuffle_ps(s, s, 0b01));
        _mm_cvtss_f32(s)
    }

    /// # Safety (all three kernels)
    /// - AVX2 + FMA must be available; [`super::kernels`] verifies this with
    ///   `is_x86_feature_detected!` before exposing these functions.
    /// - Reads stay in bounds: the vector loop covers `len/8` full 8-lane
    ///   chunks via unaligned loads (`loadu`, no alignment obligation), and
    ///   the scalar tail loop covers the remainder through safe indexing.
    #[target_feature(enable = "avx2", enable = "fma")]
    unsafe fn dot_impl(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let mut acc = _mm256_setzero_ps();
        for i in 0..chunks {
            let va = _mm256_loadu_ps(a.as_ptr().add(i * 8));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i * 8));
            acc = _mm256_fmadd_ps(va, vb, acc); // acc += va * vb
        }
        let mut sum = hsum(acc);
        for i in chunks * 8..n {
            sum += a[i] * b[i];
        }
        sum
    }

    /// # Safety: see [`dot_impl`].
    #[target_feature(enable = "avx2", enable = "fma")]
    unsafe fn l2_sq_impl(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let mut acc = _mm256_setzero_ps();
        for i in 0..chunks {
            let va = _mm256_loadu_ps(a.as_ptr().add(i * 8));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i * 8));
            let d = _mm256_sub_ps(va, vb);
            acc = _mm256_fmadd_ps(d, d, acc); // acc += d * d
        }
        let mut sum = hsum(acc);
        for i in chunks * 8..n {
            let d = a[i] - b[i];
            sum += d * d;
        }
        sum
    }

    /// # Safety: see [`dot_impl`].
    #[target_feature(enable = "avx2", enable = "fma")]
    unsafe fn dot_norms_impl(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
        let n = a.len().min(b.len());
        let chunks = n / 8;
        let mut acc_dot = _mm256_setzero_ps();
        let mut acc_na = _mm256_setzero_ps();
        let mut acc_nb = _mm256_setzero_ps();
        for i in 0..chunks {
            let va = _mm256_loadu_ps(a.as_ptr().add(i * 8));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i * 8));
            acc_dot = _mm256_fmadd_ps(va, vb, acc_dot);
            acc_na = _mm256_fmadd_ps(va, va, acc_na);
            acc_nb = _mm256_fmadd_ps(vb, vb, acc_nb);
        }
        let (mut dot, mut na, mut nb) = (hsum(acc_dot), hsum(acc_na), hsum(acc_nb));
        for i in chunks * 8..n {
            dot += a[i] * b[i];
            na += a[i] * a[i];
            nb += b[i] * b[i];
        }
        (dot, na, nb)
    }

    // Safe wrappers matching the `fn(&[f32], &[f32]) -> _` pointer type.
    // SAFETY: only reachable through `kernels()`, which has already proven
    // AVX2+FMA support on this CPU.
    pub fn dot(a: &[f32], b: &[f32]) -> f32 {
        unsafe { dot_impl(a, b) }
    }
    pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
        unsafe { l2_sq_impl(a, b) }
    }
    pub fn dot_norms(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
        unsafe { dot_norms_impl(a, b) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_known_values() {
        let a = [1.0, 2.0, 3.0];
        let b = [4.0, 5.0, 6.0];
        assert_eq!((SCALAR.dot)(&a, &b), 32.0);
        assert_eq!(SCALAR.distance(Metric::Dot, &a, &b), -32.0);
    }

    #[test]
    fn l2_known_values() {
        let a = [0.0, 0.0];
        let b = [3.0, 4.0];
        assert_eq!(SCALAR.distance(Metric::L2, &a, &b), 25.0); // squared: 5² = 25
    }

    #[test]
    fn cosine_identical_orthogonal_opposite() {
        let k = SCALAR;
        let e1 = [1.0, 0.0];
        let e2 = [0.0, 1.0];
        let neg = [-1.0, 0.0];
        assert!((k.distance(Metric::Cosine, &e1, &e1)).abs() < 1e-6); // same dir: 0
        assert!((k.distance(Metric::Cosine, &e1, &e2) - 1.0).abs() < 1e-6); // 90°: 1
        assert!((k.distance(Metric::Cosine, &e1, &neg) - 2.0).abs() < 1e-6); // 180°: 2
    }

    #[test]
    fn cosine_of_zero_vector_is_max_not_nan() {
        let z = [0.0, 0.0];
        let a = [1.0, 2.0];
        let d = SCALAR.distance(Metric::Cosine, &z, &a);
        assert!(!d.is_nan());
        assert_eq!(d, 1.0);
    }

    #[test]
    fn cosine_ignores_magnitude() {
        let a = [1.0, 2.0, 3.0];
        let big = [10.0, 20.0, 30.0];
        assert!(SCALAR.distance(Metric::Cosine, &a, &big).abs() < 1e-6);
    }

    #[test]
    fn normalize_makes_unit_length_and_dot_equals_cosine() {
        let mut a = vec![3.0, 4.0, 0.0, 1.0];
        let mut b = vec![-1.0, 2.0, 5.0, 0.5];
        let cos_before = SCALAR.distance(Metric::Cosine, &a, &b);
        normalize(&mut a);
        normalize(&mut b);
        assert!(((SCALAR.dot)(&a, &a) - 1.0).abs() < 1e-6);
        // On unit vectors, 1 - dot == cosine distance of the originals.
        let one_minus_dot = 1.0 - (SCALAR.dot)(&a, &b);
        assert!((one_minus_dot - cos_before).abs() < 1e-5);
    }

    #[test]
    fn normalize_zero_vector_is_noop() {
        let mut z = vec![0.0; 4];
        normalize(&mut z);
        assert_eq!(z, vec![0.0; 4]);
    }

    #[test]
    fn runtime_kernels_agree_with_scalar_on_a_smoke_vector() {
        // The real SIMD≈scalar guarantee lives in tests/distance_prop.rs;
        // this is a cheap sanity check that dispatch works at all.
        let k = kernels();
        let a: Vec<f32> = (0..37).map(|i| (i as f32) * 0.25 - 3.0).collect();
        let b: Vec<f32> = (0..37).map(|i| 2.0 - (i as f32) * 0.5).collect();
        for m in [Metric::L2, Metric::Cosine, Metric::Dot] {
            let (ds, dk) = (SCALAR.distance(m, &a, &b), k.distance(m, &a, &b));
            assert!(
                (ds - dk).abs() <= 1e-3 * ds.abs().max(1.0),
                "{m:?}: {ds} vs {dk}"
            );
        }
    }
}
