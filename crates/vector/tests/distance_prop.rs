//! Property tests: the SIMD kernels must agree with the scalar reference on
//! random vectors — same discipline as the B+-tree's model test against
//! `BTreeMap`, applied to floating point.
//!
//! Agreement is within a *relative* epsilon, not bit-for-bit: the AVX2 kernels
//! keep 8 partial sums and fold them at the end, which associates the
//! additions differently than the scalar left-to-right loop. IEEE 754 addition
//! is not associative, so a small rounding divergence is expected and correct.
//! (If these ever agreed exactly on all inputs, that would suggest the SIMD
//! path silently fell back to scalar.)
//!
//! Dimensions are drawn from 1..=131 so every remainder class `len % 8` is
//! exercised — the scalar tail loop after the 8-lane chunks is where an
//! off-by-one would hide.

use proptest::prelude::*;
use vector::distance::{kernels, normalize, Metric, SCALAR};

const REL_EPS: f32 = 1e-3;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= REL_EPS * a.abs().max(b.abs()).max(1.0)
}

/// A vector of finite, moderately-sized floats (keeps sums well-conditioned;
/// catastrophic cancellation is a numerics topic, not a kernel-parity topic).
fn vec_strategy(len: usize) -> impl Strategy<Value = Vec<f32>> {
    prop::collection::vec(-100.0f32..100.0, len)
}

fn pair_strategy() -> impl Strategy<Value = (Vec<f32>, Vec<f32>)> {
    (1usize..=131).prop_flat_map(|len| (vec_strategy(len), vec_strategy(len)))
}

proptest! {
    #[test]
    fn simd_dot_matches_scalar((a, b) in pair_strategy()) {
        let k = kernels();
        prop_assert!(close((k.dot)(&a, &b), (SCALAR.dot)(&a, &b)));
    }

    #[test]
    fn simd_l2_matches_scalar((a, b) in pair_strategy()) {
        let k = kernels();
        prop_assert!(close((k.l2_sq)(&a, &b), (SCALAR.l2_sq)(&a, &b)));
    }

    #[test]
    fn simd_distance_matches_scalar_for_every_metric((a, b) in pair_strategy()) {
        let k = kernels();
        for m in [Metric::L2, Metric::Cosine, Metric::Dot] {
            let (ds, dk) = (SCALAR.distance(m, &a, &b), k.distance(m, &a, &b));
            prop_assert!(close(ds, dk), "{:?}: scalar={} simd={}", m, ds, dk);
        }
    }

    #[test]
    fn distances_are_finite_and_self_distance_is_zero(a in vec_strategy(64)) {
        let k = kernels();
        // d(a, a) must be 0 for L2 and (for nonzero a) ~0 for cosine.
        prop_assert_eq!(k.distance(Metric::L2, &a, &a), 0.0);
        let d = k.distance(Metric::Cosine, &a, &a);
        prop_assert!(d.is_finite());
        if (SCALAR.dot)(&a, &a) > 1e-6 {
            prop_assert!(d.abs() < 1e-3);
        }
    }

    #[test]
    fn l2_is_symmetric((a, b) in pair_strategy()) {
        let k = kernels();
        prop_assert!(close(k.distance(Metric::L2, &a, &b), k.distance(Metric::L2, &b, &a)));
    }

    #[test]
    fn normalized_dot_orders_like_cosine((q, a, b) in (2usize..=96).prop_flat_map(|len| {
        (vec_strategy(len), vec_strategy(len), vec_strategy(len))
    })) {
        // The normalize-on-insert contract: ranking by Dot over unit vectors
        // must equal ranking by Cosine over the originals.
        let k = kernels();
        let (mut qn, mut an, mut bn) = (q.clone(), a.clone(), b.clone());
        normalize(&mut qn);
        normalize(&mut an);
        normalize(&mut bn);
        let (ca, cb) = (k.distance(Metric::Cosine, &q, &a), k.distance(Metric::Cosine, &q, &b));
        let (da, db) = (k.distance(Metric::Dot, &qn, &an), k.distance(Metric::Dot, &qn, &bn));
        // Only assert when the cosine gap is decisive enough to survive rounding.
        if (ca - cb).abs() > 1e-3 {
            prop_assert_eq!(
                ca < cb,
                da < db,
                "cosine ranks ({}, {}) but normalized dot ranks ({}, {})",
                ca,
                cb,
                da,
                db
            );
        }
    }
}
