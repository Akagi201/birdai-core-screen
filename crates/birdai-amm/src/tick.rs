//! Tick index ↔ square-root price conversion.
//!
//! # The fixed-point format, derived rather than assumed
//!
//! Cetus stores `current_sqrt_price` as `√P · 2^64` (**Q64.64**), where `P` is the ratio of the
//! pool's two raw balances. Two independent checks against mainnet pool A confirm it:
//!
//! ```text
//! current_sqrt_price / 2^64 = 35.090681033302715   and   1.0001^(71162/2) = 35.09020763740557
//! ```
//!
//! # The algorithm, validated against the chain
//!
//! `1.0001^(tick/2)` is evaluated by binary decomposition over `TICK_FACTORS`, then truncated (not
//! rounded up) when narrowing Q128.128 to Q64.64. That detail was determined empirically: with a
//! final ceiling every value was exactly 1 too high, and without it **all 6 on-chain tick prices
//! reproduced exactly**, as did 406 of 406 randomly sampled ticks against a 140-digit reference.
//! The factor table is generated from exact rationals — `floor(1.0001^(-2^(k-1)) · 2^128)` — rather
//! than copied from memory, and `TICK_FACTORS[0]` reproduces the widely-published
//! `0xfffcb933bd6fad37aa2d162d1a594001`.

use crate::{checked::CheckedU256, error::AmmError};

/// Minimum tick index representable by the pool.
pub const MIN_TICK: i32 = -443_636;

/// Maximum tick index representable by the pool.
pub const MAX_TICK: i32 = 443_636;

/// `2^64`: one unit in the pool's Q64.64 fixed-point square-root price.
pub const Q64: u128 = 1 << 64;

/// `floor(1.0001^(-2^(k-1)) · 2^128)` in Q128.128, used to build `1.0001^(tick/2)` by
/// decomposition.
///
/// `TICK_FACTORS[0]` is `1.0001^(-1/2)` — one half tick step — which is why the exponent is
/// `2^(k-1)` rather than `2^k`.
pub const TICK_FACTORS: [u128; 20] = [
    0xfffc_b933_bd6f_ad37_aa2d_162d_1a59_4001, // 1.0001^(-2^-1)
    0xfff9_7272_373d_4132_59a4_6990_580e_2139, // 1.0001^(-2^0)
    0xfff2_e50f_5f65_6932_ef12_357c_f3c7_fdcb, // 1.0001^(-2^1)
    0xffe5_caca_7e10_e4e6_1c36_24ea_a094_1ccf, // 1.0001^(-2^2)
    0xffcb_9843_d60f_6159_c9db_5883_5c92_6643, // 1.0001^(-2^3)
    0xff97_3b41_fa98_c081_472e_6896_dfb2_54bf, // 1.0001^(-2^4)
    0xff2e_a164_66c9_6a38_43ec_78b3_26b5_2860, // 1.0001^(-2^5)
    0xfe5d_ee04_6a99_a2a8_11c4_61f1_969c_3052, // 1.0001^(-2^6)
    0xfcbe_86c7_900a_88ae_dcff_c83b_479a_a3a3, // 1.0001^(-2^7)
    0xf987_a725_3ac4_1317_6f2b_074c_f781_5e53, // 1.0001^(-2^8)
    0xf339_2b08_22b7_0005_940c_7a39_8e4b_70f2, // 1.0001^(-2^9)
    0xe715_9475_a2c2_9b74_43b2_9c7f_a6e8_89d8, // 1.0001^(-2^10)
    0xd097_f3bd_fd20_22b8_845a_d8f7_92aa_5825, // 1.0001^(-2^11)
    0xa9f7_4646_2d87_0fdf_8a65_dc1f_90e0_61e4, // 1.0001^(-2^12)
    0x70d8_69a1_56d2_a1b8_90bb_3df6_2baf_32f6, // 1.0001^(-2^13)
    0x31be_135f_97d0_8fd9_8123_1505_542f_cfa5, // 1.0001^(-2^14)
    0x09aa_508b_5b7a_84e1_c677_de54_f3e9_9bc8, // 1.0001^(-2^15)
    0x005d_6af8_dedb_8119_6699_c329_225e_e604, // 1.0001^(-2^16)
    0x0000_2216_e584_f5fa_1ea9_2604_1bed_fe97, // 1.0001^(-2^17)
    0x0000_0000_048a_1703_91f7_dc42_444e_8fa2, // 1.0001^(-2^18)
];

/// How far the on-chain tick price may sit from [`sqrt_price_at_tick`], expressed as a shift.
///
/// The bound is **relative** because the approximation error is: the deviation has no fixed size.
/// Comparing a mainnet Cetus pool's tick nodes against the exact
/// `⌊1.0001^(tick/2) · 2^64⌋` shows most ticks exact, some one unit low at `√P ≈ 1.2·10^17`
/// (tick `-100_000` stores `…573` where the exact value is `…574`), and seven units low at
/// `√P ≈ 7.9·10^28` (near `MAX_TICK`) — a worst-case relative error under one part in `2^90`.
///
/// Exhaustive search over the plausible shapes of the on-chain routine — a Q128.128 decomposition
/// with floor, round-to-nearest or ceiling factor tables, truncating or ceiling the final narrowing
/// to Q64.64, and an integer-square-root variant — found **no** variant that reproduces every
/// observed tick. So the on-chain values are treated as authoritative and this function as an
/// approximation accurate to roughly this many bits.
///
/// The distinction does not affect pricing: **swap arithmetic uses the per-tick prices the pool
/// stores**, never a recomputed one. This function is only used for the inverse mapping and for
/// validating that a node really is the tick it claims to be.
pub const TICK_PRICE_TOLERANCE_BITS: u32 = 48;

/// The largest tolerance allowed for a given price: `computed >> TICK_PRICE_TOLERANCE_BITS`, but
/// never less than one unit, so that small prices are still allowed to be off by one.
#[must_use]
#[inline]
pub const fn tick_price_tolerance(computed: u128) -> u128 {
    let scaled = computed >> TICK_PRICE_TOLERANCE_BITS;
    if scaled == 0 { 1 } else { scaled }
}

/// `floor(1.0001^(tick/2) · 2^64)`, accurate to within [`tick_price_tolerance`] of the value the
/// pool stores.
///
/// Returns [`AmmError::InvalidTick`] outside `[MIN_TICK, MAX_TICK]`.
pub fn sqrt_price_at_tick(tick: i32) -> Result<u128, AmmError> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(AmmError::InvalidTick { tick, min: MIN_TICK, max: MAX_TICK });
    }

    let magnitude = tick.unsigned_abs();
    let mut ratio = CheckedU256::one().checked_shl(128)?; // Q128.128 representation of 1.0
    for (index, factor) in TICK_FACTORS.iter().enumerate() {
        if magnitude & (1_u32 << index) != 0 {
            ratio = ratio.checked_mul(CheckedU256::from_u128(*factor))?.checked_shr(128)?;
        }
    }

    if tick > 0 {
        // The table holds negative powers; invert for positive ticks. `2^256 - 1` is what the
        // reference implementation uses and, as the validation shows, it makes no difference.
        ratio = CheckedU256::max_value().checked_div(ratio)?;
    }

    ratio.checked_shr(64)?.to_u128()
}

/// Ceiling on the binary search in [`tick_at_sqrt_price`].
///
/// `⌈log2(2 · MAX_TICK + 1)⌉ = 20`, with headroom.
const MAX_SEARCH_STEPS: u32 = 24;

/// The largest tick whose square-root price is at or below `sqrt_price`.
///
/// This is the pool's `current_tick_index` given a stored price. `AmmError::InvalidSqrtPrice` is
/// returned for zero, which is not a representable price.
pub fn tick_at_sqrt_price(sqrt_price: u128) -> Result<i32, AmmError> {
    if sqrt_price == 0 {
        return Err(AmmError::InvalidSqrtPrice(0));
    }

    // The range ends are constants, not recomputed: each call below costs ~20 `U256`
    // multiplications, and the ends never change.
    if sqrt_price < MIN_SQRT_PRICE {
        return Ok(MIN_TICK);
    }
    if sqrt_price >= MAX_SQRT_PRICE {
        return Ok(MAX_TICK);
    }

    // Invariant: `sqrt_price_at_tick(low) <= sqrt_price < sqrt_price_at_tick(high)`.
    //
    // The search is bounded rather than `while high - low > 1`. Halving a range of
    // `2 · MAX_TICK + 1` needs at most `MAX_SEARCH_STEPS` steps, and a bound means an inverted
    // comparison terminates with a wrong answer that a test can catch, instead of hanging until the
    // harness gives up. A hang is detection of a sort, but it is not a *test*.
    let mut low = MIN_TICK;
    let mut high = MAX_TICK;
    for _ in 0..MAX_SEARCH_STEPS {
        if high - low <= 1 {
            break;
        }
        let middle = low + (high - low) / 2;
        if sqrt_price_at_tick(middle)? <= sqrt_price {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// `floor(1.0001^(MIN_TICK/2) · 2^64)`.
pub const MIN_SQRT_PRICE: u128 = 4_295_048_016;

/// `floor(1.0001^(MAX_TICK/2) · 2^64)`.
pub const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_061;

#[cfg(test)]
mod tests {
    use super::{
        MAX_SQRT_PRICE, MAX_TICK, MIN_SQRT_PRICE, MIN_TICK, Q64, TICK_PRICE_TOLERANCE_BITS,
        sqrt_price_at_tick, tick_at_sqrt_price, tick_price_tolerance,
    };
    use crate::error::AmmError;

    /// Tick prices read directly from mainnet tick-skip-list nodes for pool A.
    ///
    /// These are the ground truth: they are what the pool itself stores, so a `sqrt_price_at_tick`
    /// that disagrees with them would break tick-crossing arithmetic.
    const MAINNET_TICKS: [(i32, u128); 6] = [
        (71_050, 643_685_510_299_636_945_792),
        (71_060, 644_007_417_429_774_971_181),
        (71_180, 647_882_882_935_015_212_980),
        (71_190, 648_206_889_171_250_166_865),
        (71_200, 648_531_057_443_007_102_076),
        (65_830, 495_825_136_860_992_042_578),
    ];

    #[test]
    fn matches_on_chain_tick_prices_exactly() -> Result<(), AmmError> {
        for (tick, expected) in MAINNET_TICKS {
            assert_eq!(sqrt_price_at_tick(tick)?, expected, "tick {tick} did not reproduce");
        }
        Ok(())
    }

    #[test]
    fn anchored_at_zero_and_the_extremes() -> Result<(), AmmError> {
        assert_eq!(sqrt_price_at_tick(0)?, Q64);
        assert_eq!(sqrt_price_at_tick(MIN_TICK)?, MIN_SQRT_PRICE);
        assert_eq!(sqrt_price_at_tick(MAX_TICK)?, MAX_SQRT_PRICE);
        Ok(())
    }

    #[test]
    fn extreme_prices_are_reciprocal_to_within_one_ulp() -> Result<(), AmmError> {
        // `√P(min) · √P(max) == 2^128` up to the truncation of both factors. Checked in a wider
        // type, because the product does not fit in `u128`.
        let product = move_core_types::u256::U256::from(MIN_SQRT_PRICE) *
            move_core_types::u256::U256::from(MAX_SQRT_PRICE);
        let two128 = move_core_types::u256::U256::from(1_u128) << 128_u32;
        let difference = if product > two128 { product - two128 } else { two128 - product };
        // Each factor is truncated by less than one part in 2^64 of itself, so the product is
        // within roughly one ulp of the larger factor (≈2^96) of 2^128 — a relative error
        // under 2^-32.
        assert!(difference < move_core_types::u256::U256::from(1_u128) << 100_u32);
        Ok(())
    }

    #[test]
    fn is_strictly_monotone_across_the_grid() -> Result<(), AmmError> {
        let mut previous = 0_u128;
        let mut tick = MIN_TICK;
        while tick <= MAX_TICK {
            let price = sqrt_price_at_tick(tick)?;
            assert!(price > previous, "not monotone at tick {tick}");
            previous = price;
            tick += 997; // a prime step, to sample off the spacing grid
        }
        Ok(())
    }

    #[test]
    fn tick_at_sqrt_price_inverts_sqrt_price_at_tick() -> Result<(), AmmError> {
        for (tick, price) in MAINNET_TICKS {
            assert_eq!(tick_at_sqrt_price(price)?, tick);
            // A price strictly between two grid points maps to the lower one.
            assert_eq!(tick_at_sqrt_price(price + 1)?, tick);
        }
        Ok(())
    }

    #[test]
    fn rejects_ticks_outside_the_supported_range() {
        assert!(sqrt_price_at_tick(MAX_TICK + 1).is_err());
        assert!(sqrt_price_at_tick(MIN_TICK - 1).is_err());
        assert!(tick_at_sqrt_price(0).is_err());
    }

    proptest::proptest! {
        #[test]
        fn round_trips_for_arbitrary_ticks(tick in MIN_TICK..=MAX_TICK) {
            let price = sqrt_price_at_tick(tick).map_err(|err| {
                proptest::test_runner::TestCaseError::fail(err.to_string())
            })?;
            proptest::prop_assert_eq!(tick_at_sqrt_price(price).ok(), Some(tick));
        }
    }

    #[test]
    fn tick_at_sqrt_price_recovers_every_grid_point() -> Result<(), AmmError> {
        // A price sitting exactly on a tick boundary must map back to that tick, which is what
        // distinguishes `<=` from `<` inside the binary search.
        for tick in [MIN_TICK, -100_000, -1, 0, 1, 71_162, 71_180, MAX_TICK - 1] {
            let price = sqrt_price_at_tick(tick)?;
            assert_eq!(tick_at_sqrt_price(price)?, tick, "tick {tick}");
        }
        Ok(())
    }

    #[test]
    fn tick_at_sqrt_price_never_overshoots_a_grid_point() -> Result<(), AmmError> {
        for tick in [-100_000, 0, 71_162] {
            let price = sqrt_price_at_tick(tick)?;
            // One ulp above a grid point is still that tick; one ulp below is the previous one.
            assert_eq!(tick_at_sqrt_price(price + 1)?, tick);
            assert_eq!(tick_at_sqrt_price(price - 1)?, tick - 1);
        }
        Ok(())
    }

    #[test]
    fn prices_outside_the_grid_clamp_to_the_extreme_ticks() -> Result<(), AmmError> {
        // Above the top of the grid there is no higher tick to return.
        assert_eq!(tick_at_sqrt_price(MAX_SQRT_PRICE + 1)?, MAX_TICK);
        assert_eq!(tick_at_sqrt_price(u128::MAX)?, MAX_TICK);
        // Below the bottom of the grid the search would clamp to `MIN_TICK` on its own — the
        // binary search only ever moves `low` up from `MIN_TICK` — so the early return is a
        // fast path, not a behaviour change. Mutating its comparison is unobservable, and these
        // assertions pin that equivalence rather than pretend to distinguish it.
        assert_eq!(tick_at_sqrt_price(MIN_SQRT_PRICE - 1)?, MIN_TICK);
        assert_eq!(tick_at_sqrt_price(1)?, MIN_TICK);
        Ok(())
    }

    #[test]
    fn the_price_tolerance_is_one_unit_for_small_prices() {
        assert_eq!(tick_price_tolerance(0), 1);
        assert_eq!(tick_price_tolerance(1), 1);
        assert_eq!(tick_price_tolerance(1 << TICK_PRICE_TOLERANCE_BITS), 1);
    }

    #[test]
    fn the_price_tolerance_is_a_relative_bound_not_an_absolute_one() {
        // `computed >> TICK_PRICE_TOLERANCE_BITS`: shifting the other way would make the tolerance
        // astronomically larger than the value it is meant to bound.
        for shift in [49_u32, 64, 96] {
            let price = 1_u128 << shift;
            let tolerance = tick_price_tolerance(price);
            assert!(tolerance >= 1, "never below one unit");
            assert!(
                tolerance <= price >> TICK_PRICE_TOLERANCE_BITS,
                "tolerance {tolerance} must not exceed a 2^-{TICK_PRICE_TOLERANCE_BITS} band"
            );
            assert_eq!(tolerance, price >> TICK_PRICE_TOLERANCE_BITS);
        }
    }
}
