//! Tests for the read-only accessors on [`Ticks`] and [`SizeSkew`].
//!
//! These small predicates are what a caller uses to decide whether an index can be trusted, so
//! getting one inverted would be a silent-wrong-answer bug rather than a crash — exactly the kind
//! of thing worth pinning down directly.

use birdai_move::{I32, I128};

use crate::{SizeSkew, Tick, TickNode, Ticks};

fn node(index: i32, sqrt_price: u128, liquidity_net: i128) -> TickNode {
    TickNode {
        score: (index + 443_636) as u64,
        nexts: vec![],
        prev: None,
        tick: Tick {
            index: I32::new(index),
            sqrt_price,
            liquidity_net: I128::new(liquidity_net),
            liquidity_gross: 0,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            points_growth_outside: 0,
            rewards_growth_outside: vec![],
        },
    }
}

#[test]
fn size_skew_delta_is_signed_and_directional() {
    let grew = SizeSkew { declared: 650, observed: 653 };
    assert_eq!(grew.delta(), 3);
    let shrank = SizeSkew { declared: 653, observed: 650 };
    assert_eq!(shrank.delta(), -3);
    let equal = SizeSkew { declared: 650, observed: 650 };
    assert_eq!(equal.delta(), 0);
}

#[test]
fn from_children_reports_a_skew_only_when_the_counts_differ() -> Result<(), crate::TickError> {
    let ticks = [node(71_180, 647_882_882_935_015_212_980, 1)];

    let (index, skew) = Ticks::from_children(None, 1, ticks.clone())?;
    assert_eq!(skew, None, "matching counts mean no skew");
    assert!(index.is_complete());
    assert_eq!(index.declared_size(), Some(1));

    let (index, skew) = Ticks::from_children(None, 3, ticks)?;
    assert_eq!(skew, Some(SizeSkew { declared: 3, observed: 1 }));
    assert!(!index.is_complete());
    Ok(())
}

#[test]
fn declared_size_and_is_complete_track_the_declared_count() -> Result<(), crate::TickError> {
    let strict = Ticks::new(None, None, [node(71_180, 647_882_882_935_015_212_980, 1)])?;
    assert_eq!(strict.declared_size(), None, "no count was supplied");
    // With no declared count there is nothing to disagree with.
    assert!(strict.is_complete());
    Ok(())
}

#[test]
fn the_price_deviation_histogram_counts_each_bucket() -> Result<(), crate::TickError> {
    let exact_1 = node(71_180, 647_882_882_935_015_212_980, 0);
    let exact_2 = node(71_190, 648_206_889_171_250_166_865, 0);
    // One unit below the exact grid price, as a minority of mainnet ticks are.
    let low_one = node(71_200, 648_531_057_443_007_102_076 - 1, 0);
    let index = Ticks::new(None, None, [exact_1, exact_2, low_one])?;

    assert_eq!(index.exact_price_nodes(), 2);
    assert_eq!(index.max_price_deviation(), 1);
    assert_eq!(index.price_deviations().get(&0), Some(&2));
    assert_eq!(index.price_deviations().get(&-1), Some(&1));
    assert_eq!(index.price_deviations().get(&1), None);
    Ok(())
}

#[test]
fn an_index_with_no_declared_count_answers_completeness_optimistically() {
    let empty = Ticks::default();
    assert!(empty.is_complete());
    assert_eq!(empty.declared_size(), None);
    assert_eq!(empty.max_price_deviation(), 0);
    assert_eq!(empty.exact_price_nodes(), 0);
    assert!(empty.price_deviations().is_empty());
}
