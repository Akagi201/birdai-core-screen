//! Concentrated-liquidity swap arithmetic.
//!
//! # Conventions
//!
//! * Token A and token B are the pool's two coin types in declaration order.
//! * `sqrt_price` is Q64.64 and equals `√(b_v / a_v)`, with `a_v`/`b_v` the *virtual* reserves.
//! * `liquidity` is `L = √(a_v · b_v)`, constant while the price stays inside one tick range.
//! * Selling B pushes the price **up**; selling A pushes it **down**.
//!
//! # Rounding
//!
//! Every rounding direction matches the pool's, and it is the pool's own combination of the two:
//! the fee is floored, the output is floored (the pool never pays out more than it owes), and
//! anything that moves *towards* the pool — the amount the trader owes for a step, and the price an
//! A-in step reaches — is ceiled, so a step can never take the pool further than the input paid
//! for. [`crate`] documents why this reproduces the chain exactly.

use crate::{checked::CheckedU256, error::AmmError, tick::Q64};

/// The denominator of `fee_rate`: the pool stores fees in millionths.
pub const FEE_DENOMINATOR: u64 = 1_000_000;

/// Split a gross input into the amount that participates in the swap and the fee taken from it.
///
/// The fee is rounded **down**, matching the pool. Returns the net input first.
#[inline]
pub fn take_fee(amount_in: u128, fee_rate: u64) -> Result<(u128, u128), AmmError> {
    if fee_rate >= FEE_DENOMINATOR {
        return Err(AmmError::InvalidFeeRate { rate: fee_rate });
    }
    let fee = CheckedU256::from_u128(amount_in)
        .checked_mul(CheckedU256::from_u64(fee_rate))?
        .checked_div(CheckedU256::from_u64(FEE_DENOMINATOR))?
        .to_u128()?;
    let net = amount_in.checked_sub(fee).ok_or(AmmError::Overflow { op: "fee exceeds input" })?;
    Ok((net, fee))
}

/// `Δa = L · (1/√P_lo − 1/√P_hi)`, the amount of token A held by liquidity `L` between two prices.
///
/// Rounded down by default and up when `round_up`, exactly as the pool does when it computes what
/// it is owed versus what it pays out.
#[inline]
pub fn delta_a(
    sqrt_a: u128,
    sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<u128, AmmError> {
    let (low, high) = ordered(sqrt_a, sqrt_b);
    if low == 0 {
        return Err(AmmError::InvalidSqrtPrice(0));
    }
    if liquidity == 0 || low == high {
        return Ok(0);
    }

    let numerator = CheckedU256::from_u128(liquidity)
        .checked_shl(64)?
        .checked_mul(CheckedU256::from_u128(high - low))?;
    let high_u256 = CheckedU256::from_u128(high);
    let low_u256 = CheckedU256::from_u128(low);
    let step = if round_up {
        numerator.checked_div_ceil(high_u256)?
    } else {
        numerator.checked_div(high_u256)?
    };
    let result =
        if round_up { step.checked_div_ceil(low_u256)? } else { step.checked_div(low_u256)? };
    result.to_u128()
}

/// `Δb = L · (√P_hi − √P_lo) / 2^64`, the amount of token B held by liquidity `L` between two
/// prices.
#[inline]
pub fn delta_b(
    sqrt_a: u128,
    sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Result<u128, AmmError> {
    let (low, high) = ordered(sqrt_a, sqrt_b);
    if liquidity == 0 || low == high {
        return Ok(0);
    }

    let product =
        CheckedU256::from_u128(liquidity).checked_mul(CheckedU256::from_u128(high - low))?;
    let scale = CheckedU256::one().checked_shl(64)?;
    let result =
        if round_up { product.checked_div_ceil(scale)? } else { product.checked_div(scale)? };
    result.to_u128()
}

/// The square-root price reached after selling `amount_b` of token B (price moving up).
///
/// Rounded **down**, so the price target never overshoots what the input can pay for.
#[inline]
pub fn next_sqrt_price_up(
    sqrt_price: u128,
    liquidity: u128,
    amount_b: u128,
) -> Result<u128, AmmError> {
    if liquidity == 0 {
        return Err(AmmError::ZeroLiquidity);
    }
    let increment = CheckedU256::from_u128(amount_b)
        .checked_shl(64)?
        .checked_div(CheckedU256::from_u128(liquidity))?
        .to_u128()?;
    sqrt_price.checked_add(increment).ok_or(AmmError::Overflow { op: "next_sqrt_price_up" })
}

/// The square-root price reached after selling `amount_a` of token A (price moving down).
///
/// Rounded **up**, so the price target never undershoots what the input can pay for. This is the
/// exact form of the pool's `⌈L·√P / (L + amount·√P/2^64)⌉`; because the intermediates are computed
/// in 256 bits there is no need for the overflow fallback the 256-bit reference implementation
/// carries.
#[inline]
pub fn next_sqrt_price_down(
    sqrt_price: u128,
    liquidity: u128,
    amount_a: u128,
) -> Result<u128, AmmError> {
    if liquidity == 0 {
        return Err(AmmError::ZeroLiquidity);
    }
    // Everything stays in 256 bits, so there is no need for the "did the `L ≪ 64` overflow" branch
    // the reference implementation carries for a 256-bit numerator.
    let numerator = CheckedU256::from_u128(liquidity).checked_shl(64)?;
    let denominator = numerator.checked_add(
        CheckedU256::from_u128(amount_a).checked_mul(CheckedU256::from_u128(sqrt_price))?,
    )?;
    numerator
        .checked_mul(CheckedU256::from_u128(sqrt_price))?
        .checked_div_ceil(denominator)?
        .to_u128()
}

/// An initialised tick boundary, as the price sees it while travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// The tick index.
    pub tick: i32,
    /// The tick's square-root price, Q64.64.
    pub sqrt_price: u128,
    /// Liquidity added to the active range when the price crosses this tick **upwards**.
    pub liquidity_net: i128,
}

/// Where the next initialised tick lies, in the direction of travel.
///
/// Implemented by `birdai-tick` over the pool's skip list; kept as a trait here so that the
/// arithmetic crate stays free of I/O and of the protocol's storage layout.
pub trait TickSource {
    /// The nearest initialised tick **strictly above** `tick`.
    fn next_boundary_up(&self, tick: i32) -> Option<Boundary>;

    /// The nearest initialised tick **strictly below** `tick`.
    fn next_boundary_down(&self, tick: i32) -> Option<Boundary>;
}

/// Which way value is flowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Token A in, token B out; the price moves down.
    AtoB,
    /// Token B in, token A out; the price moves up.
    BtoA,
}

/// What one step of a swap did, with the price and range it moved through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// Net input consumed by this step (fee already removed).
    pub amount_in: u128,
    /// Output produced by this step.
    pub amount_out: u128,
    /// Price before the step.
    pub sqrt_price_start: u128,
    /// Price after the step.
    pub sqrt_price_end: u128,
    /// Active liquidity used by the step.
    pub liquidity: u128,
    /// True when the step reached its price target — a tick boundary or the swap's price
    /// limit — rather than running out of input.
    pub crossed: bool,
}

/// The result of a whole swap, with the per-step breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapResult {
    /// Gross input, including the fee.
    pub amount_in: u128,
    /// Fee charged on the consumed input (floored, proportional on truncation).
    pub fee: u128,
    /// Net output.
    pub amount_out: u128,
    /// Price before the swap.
    pub sqrt_price_start: u128,
    /// Price after the swap.
    pub sqrt_price_end: u128,
    /// Current tick before the swap.
    pub tick_start: i32,
    /// The last initialised tick crossed, or the starting tick when none was crossed.
    ///
    /// This is *not* `tick_at_sqrt_price(sqrt_price_end)`: the end price cannot be mapped back
    /// through the approximate tick math without risking an off-by-one against the stored
    /// per-tick prices, so the swap reports where its boundaries took it instead.
    pub tick_end: i32,
    /// Active liquidity before the swap.
    pub liquidity_start: u128,
    /// Active liquidity after the swap.
    pub liquidity_end: u128,
    /// True when the swap ran out of input rather than reaching its price limit.
    pub input_consumed: bool,
    /// One entry per step; a single-tick swap has exactly one.
    pub steps: Vec<Step>,
}

impl SwapResult {
    /// True when the swap stayed inside a single tick range, so `liquidity` never changed.
    #[must_use]
    pub const fn is_single_step(&self) -> bool {
        self.steps.len() == 1
    }
}

/// Price the input of an exact-in step would reach, whether it crosses the limit or not.
#[inline]
fn next_price(
    direction: Direction,
    sqrt_price: u128,
    liquidity: u128,
    amount_in: u128,
    limit: u128,
) -> Result<(u128, bool), AmmError> {
    // Compute the price the whole remaining input can reach, then decide whether the price limit
    // cuts the step short.
    let price_from_amount = match direction {
        Direction::BtoA => next_sqrt_price_up(sqrt_price, liquidity, amount_in)?,
        Direction::AtoB => next_sqrt_price_down(sqrt_price, liquidity, amount_in)?,
    };
    let crosses = match direction {
        Direction::BtoA => price_from_amount >= limit,
        Direction::AtoB => price_from_amount <= limit,
    };
    Ok(if crosses { (limit, true) } else { (price_from_amount, false) })
}

/// The pool state a swap starts from.
///
/// Grouped so that a caller cannot silently transpose two same-typed arguments — `sqrt_price` and
/// `liquidity` are both `u128` and swapping them produces a plausible, wrong answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    /// Square-root price in Q64.64.
    pub sqrt_price: u128,
    /// Active liquidity `L`.
    pub liquidity: u128,
    /// The tick the price currently sits in.
    pub tick: i32,
    /// Fee in millionths.
    pub fee_rate: u64,
}

/// Run an exact-input swap, walking tick boundaries.
///
/// `price_limit` bounds the whole swap (the same price limit a real transaction would carry);
/// `sqrt_price_at_tick(MAX_TICK)`/`(MIN_TICK)` is the natural choice. `max_crossings` guards
/// against a malformed tick index turning a swap into an unbounded loop.
///
/// Stops early when the price limit is reached, when the input is exhausted, or when the next
/// boundary cannot be resolved.
pub fn swap_exact_in(
    source: &dyn TickSource,
    direction: Direction,
    state: PoolState,
    amount_in: u128,
    price_limit: u128,
    max_crossings: u32,
) -> Result<SwapResult, AmmError> {
    let PoolState { sqrt_price, liquidity, tick, fee_rate } = state;
    // Reject degenerate state and an unreachable limit up front, so the loop below never has to
    // interpret them: a zero price or zero liquidity cannot be stepped from, and a limit behind
    // the price would otherwise walk the price backwards through `next_price`.
    if sqrt_price == 0 {
        return Err(AmmError::InvalidSqrtPrice(0));
    }
    if liquidity == 0 {
        return Err(AmmError::ZeroLiquidity);
    }
    let limit_reachable = match direction {
        Direction::BtoA => price_limit >= sqrt_price,
        Direction::AtoB => price_limit <= sqrt_price,
    };
    if !limit_reachable {
        return Err(AmmError::UnreachablePriceLimit { current: sqrt_price, limit: price_limit });
    }
    let (net_input, fee) = take_fee(amount_in, fee_rate)?;
    let sqrt_price_start = sqrt_price;
    let liquidity_start = liquidity;
    let tick_start = tick;

    let mut sqrt_price = sqrt_price;
    let mut liquidity = liquidity;
    let mut tick = tick;
    let mut remaining = net_input;
    let mut amount_out = 0_u128;
    let mut steps = Vec::new();
    let mut crossings = 0_u32;

    while remaining > 0 {
        // The swap has reached the price it was allowed to move the pool to.
        if sqrt_price == price_limit {
            break;
        }

        let boundary = match direction {
            Direction::BtoA => source.next_boundary_up(tick),
            Direction::AtoB => source.next_boundary_down(tick),
            // A stale source can hand back a boundary behind the price; stepping to it would
            // move the price backwards, so it is ignored rather than trusted.
        }
        .filter(|boundary| is_ahead(direction, boundary.sqrt_price, sqrt_price));
        // ponytail: clamp the boundary to the swap's price limit so a stale/far boundary
        // can never walk the price past what the transaction allowed.
        let raw_limit = boundary.map_or(price_limit, |boundary| boundary.sqrt_price);
        let limit = match direction {
            Direction::BtoA => raw_limit.min(price_limit),
            Direction::AtoB => raw_limit.max(price_limit),
        };

        let (next, reached_limit) = next_price(direction, sqrt_price, liquidity, remaining, limit)?;
        if next == sqrt_price {
            return Err(AmmError::NoProgress { remaining });
        }

        let (owed, out) = match direction {
            Direction::BtoA => (
                delta_b(sqrt_price, next, liquidity, true)?,
                delta_a(sqrt_price, next, liquidity, false)?,
            ),
            Direction::AtoB => (
                delta_a(next, sqrt_price, liquidity, true)?,
                delta_b(next, sqrt_price, liquidity, false)?,
            ),
        };

        // When the step is not cut short, the whole remaining input is consumed by construction.
        // When it is, the trader pays what it costs to reach the boundary and the rest carries on.
        let consumed = if reached_limit { owed.min(remaining) } else { remaining };

        amount_out =
            amount_out.checked_add(out).ok_or(AmmError::Overflow { op: "accumulate output" })?;

        steps.push(Step {
            amount_in: consumed,
            amount_out: out,
            sqrt_price_start: sqrt_price,
            sqrt_price_end: next,
            liquidity,
            crossed: reached_limit,
        });

        sqrt_price = next;
        remaining =
            remaining.checked_sub(consumed).ok_or(AmmError::Overflow { op: "consume input" })?;

        if !reached_limit {
            break;
        }

        // Landing on the price limit is *not* crossing a tick. When the boundary lies beyond the
        // limit, `limit` was clamped to the limit and the price never reached the tick, so its
        // liquidity must not be applied and its tick must not be reported as the one in force.
        let Some(boundary) = boundary.filter(|boundary| boundary.sqrt_price == sqrt_price) else {
            // Reached the swap's price limit rather than a tick; nothing more to do.
            break;
        };

        crossings += 1;
        if crossings > max_crossings {
            return Err(AmmError::TooManyCrossings { limit: max_crossings });
        }

        // `Boundary::liquidity_net` is defined upwards; crossing downwards applies the negation.
        let net = match direction {
            Direction::BtoA => boundary.liquidity_net,
            Direction::AtoB => boundary
                .liquidity_net
                .checked_neg()
                .ok_or(AmmError::Overflow { op: "negate liquidity net" })?,
        };
        liquidity = apply_liquidity_change(liquidity, net)?;
        tick = boundary.tick;

        if remaining == 0 {
            break;
        }
    }

    // Fee is charged only on consumed input; truncated swaps refund the rest. The `net_input == 0`
    // arm is not redundant with `remaining == 0` — it guards the division below — but the pair is
    // equivalent to `remaining == 0` alone: `remaining <= net_input`, so `net_input == 0` implies
    // `remaining == 0` and pays the same zero fee, and `remaining == 0` with a non-zero `net_input`
    // takes the refund branch only to refund `fee · 0 / net_input = 0`. Mutating `||` to `&&`
    // therefore changes nothing observable; the branch order is what the tests pin.
    let fee_charged = if remaining == 0 || net_input == 0 {
        fee
    } else {
        let refundable = CheckedU256::from_u128(fee)
            .checked_mul(CheckedU256::from_u128(remaining))?
            .checked_div(CheckedU256::from_u128(net_input))?
            .to_u128()?;
        fee.checked_sub(refundable).ok_or(AmmError::Overflow { op: "fee refund" })?
    };

    Ok(SwapResult {
        amount_in,
        fee: fee_charged,
        amount_out,
        sqrt_price_start,
        sqrt_price_end: sqrt_price,
        tick_start,
        tick_end: tick,
        liquidity_start,
        liquidity_end: liquidity,
        input_consumed: remaining == 0,
        steps,
    })
}

/// Apply the signed liquidity change of a tick boundary.
#[inline]
pub fn apply_liquidity_change(liquidity: u128, liquidity_net: i128) -> Result<u128, AmmError> {
    if liquidity_net >= 0 {
        liquidity
            .checked_add(liquidity_net.unsigned_abs())
            .ok_or(AmmError::Overflow { op: "add liquidity net" })
    } else {
        liquidity
            .checked_sub(liquidity_net.unsigned_abs())
            .ok_or(AmmError::Overflow { op: "subtract liquidity net" })
    }
}

/// True when a boundary lies strictly ahead of the price in the direction of travel.
const fn is_ahead(direction: Direction, boundary_price: u128, sqrt_price: u128) -> bool {
    match direction {
        Direction::BtoA => boundary_price > sqrt_price,
        Direction::AtoB => boundary_price < sqrt_price,
    }
}

const fn ordered(a: u128, b: u128) -> (u128, u128) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The scale factor of the Q64.64 fixed-point price, re-exported for callers that work in price
/// space rather than in square-root-price space.
///
/// ponytail: alias of [`crate::tick::Q64`]; prefer `Q64` in new code.
pub const PRICE_SCALE: u128 = Q64;

#[cfg(test)]
mod tests {
    use super::{
        Boundary, Direction, PoolState, SwapResult, TickSource, delta_a, delta_b,
        next_sqrt_price_down, next_sqrt_price_up, swap_exact_in, take_fee,
    };
    use crate::{
        error::AmmError,
        tick::{MAX_SQRT_PRICE, MAX_TICK, MIN_SQRT_PRICE, MIN_TICK, Q64, sqrt_price_at_tick},
    };

    /// The pool state transaction `T` read, verified against mainnet.
    ///
    /// ```text
    /// 0x51e883ba…::pool::Pool<USDC, SUI> @ version 995150484
    /// ```
    const T_LIQUIDITY: u128 = 120_115_891_674_982;
    const T_SQRT_PRICE: u128 = 647_308_812_393_509_050_120;
    const T_TICK: i32 = 71_162;
    const T_FEE_RATE: u64 = 500;
    const T_AMOUNT_IN: u128 = 100_000_000_000;
    const T_AMOUNT_OUT: u128 = 81_168_759;
    /// The next initialised tick above the current one, from the pool's tick skip list.
    const T_NEXT_TICK: i32 = 71_180;
    const T_NEXT_SQRT_PRICE: u128 = 647_882_882_935_015_212_980;

    /// A tick source with exactly one boundary, mirroring the mainnet state around `T`.
    struct SingleBoundary;

    impl TickSource for SingleBoundary {
        fn next_boundary_up(&self, tick: i32) -> Option<Boundary> {
            (tick < T_NEXT_TICK).then_some(Boundary {
                tick: T_NEXT_TICK,
                sqrt_price: T_NEXT_SQRT_PRICE,
                liquidity_net: 212_759_778_363,
            })
        }

        fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
            None
        }
    }

    struct EmptySource;

    impl TickSource for EmptySource {
        fn next_boundary_up(&self, _tick: i32) -> Option<Boundary> {
            None
        }

        fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
            None
        }
    }

    /// The pre-state transaction T read, as the arithmetic crate wants it.
    const T_STATE: PoolState = PoolState {
        sqrt_price: T_SQRT_PRICE,
        liquidity: T_LIQUIDITY,
        tick: T_TICK,
        fee_rate: T_FEE_RATE,
    };

    fn run_t(source: &dyn TickSource) -> Result<SwapResult, AmmError> {
        swap_exact_in(source, Direction::BtoA, T_STATE, T_AMOUNT_IN, MAX_SQRT_PRICE, 8)
    }

    #[test]
    fn fee_matches_the_transaction() -> Result<(), AmmError> {
        let (net, fee) = take_fee(T_AMOUNT_IN, T_FEE_RATE)?;
        assert_eq!(fee, 50_000_000);
        assert_eq!(net, 99_950_000_000);
        Ok(())
    }

    #[test]
    fn output_matches_the_chain_exactly() -> Result<(), AmmError> {
        let result = run_t(&SingleBoundary)?;
        assert_eq!(
            result.amount_out, T_AMOUNT_OUT,
            "recomputed output must match the on-chain result"
        );
        assert_eq!(result.fee, 50_000_000);
        assert_eq!(result.liquidity_start, T_LIQUIDITY);
        assert_eq!(result.liquidity_end, T_LIQUIDITY, "no tick was crossed");
        assert!(result.is_single_step());
        assert!(result.input_consumed);
        Ok(())
    }

    #[test]
    fn the_step_does_not_reach_the_next_initialised_tick() -> Result<(), AmmError> {
        let result = run_t(&SingleBoundary)?;
        assert!(
            result.sqrt_price_end < T_NEXT_SQRT_PRICE,
            "price {} should stay below the boundary {}",
            result.sqrt_price_end,
            T_NEXT_SQRT_PRICE
        );
        Ok(())
    }

    #[test]
    fn a_price_floor_instead_of_a_tick_gives_the_same_answer() -> Result<(), AmmError> {
        let without_boundary = run_t(&EmptySource)?;
        let with_boundary = run_t(&SingleBoundary)?;
        assert_eq!(without_boundary.amount_out, with_boundary.amount_out);
        assert_eq!(without_boundary.sqrt_price_end, with_boundary.sqrt_price_end);
        Ok(())
    }

    #[test]
    fn intermediate_values_are_the_ones_expected() -> Result<(), AmmError> {
        let delta =
            super::next_sqrt_price_up(T_SQRT_PRICE, T_LIQUIDITY, 99_950_000_000)? - T_SQRT_PRICE;
        assert_eq!(delta, 15_349_776_323_987_364);
        assert_eq!(T_SQRT_PRICE + delta, 647_324_162_169_833_037_484);
        Ok(())
    }

    /// Rounding *up* the price step is a one-unit difference that cannot move the output.
    #[test]
    fn result_is_insensitive_to_the_price_step_rounding() -> Result<(), AmmError> {
        let floor_delta =
            super::next_sqrt_price_up(T_SQRT_PRICE, T_LIQUIDITY, 99_950_000_000)? - T_SQRT_PRICE;
        let ceil_delta = floor_delta + 1;
        let end = T_SQRT_PRICE + ceil_delta;
        let out = delta_a(T_SQRT_PRICE, end, T_LIQUIDITY, false)?;
        assert_eq!(out, T_AMOUNT_OUT);
        Ok(())
    }

    #[test]
    fn delta_helpers_agree_with_the_closed_form() -> Result<(), AmmError> {
        // Small, exactly representable case: L = 2^64 means a_v = 2^64/√P and b_v = 2^64·√P.
        let low = Q64; // √P = 1.0
        let high = 2 * Q64; // √P = 2.0
        let liquidity = Q64;
        // Δa = L·(1/1 − 1/2) = L/2
        assert_eq!(delta_a(low, high, liquidity, false)?, liquidity / 2);
        // Δb = L·(2 − 1) = L
        assert_eq!(delta_b(low, high, liquidity, false)?, liquidity);
        Ok(())
    }

    #[test]
    fn rounding_always_favours_the_pool() -> Result<(), AmmError> {
        let liquidity = 1_234_567_890_123_u128;
        let low = 987_654_321_000_u128;
        let high = low + 1;
        // A one-ulp price range: the pool must never pay out more than it takes in.
        let owed_up = delta_a(low, high, liquidity, true)?;
        let paid_down = delta_a(low, high, liquidity, false)?;
        assert!(owed_up >= paid_down);
        let owed_up_b = delta_b(low, high, liquidity, true)?;
        let paid_down_b = delta_b(low, high, liquidity, false)?;
        assert!(owed_up_b >= paid_down_b);
        Ok(())
    }

    #[test]
    fn rejects_zero_liquidity() {
        assert!(matches!(next_sqrt_price_up(Q64, 0, 1), Err(AmmError::ZeroLiquidity)));
        assert!(matches!(next_sqrt_price_down(Q64, 0, 1), Err(AmmError::ZeroLiquidity)));
    }

    #[test]
    fn rejects_a_fee_rate_at_or_above_the_denominator() {
        assert!(matches!(
            take_fee(1_000, 1_000_000),
            Err(AmmError::InvalidFeeRate { rate: 1_000_000 })
        ));
        assert!(matches!(
            take_fee(1_000, 1_000_001),
            Err(AmmError::InvalidFeeRate { rate: 1_000_001 })
        ));
    }

    #[test]
    fn zero_input_takes_no_fee() -> Result<(), AmmError> {
        assert_eq!(take_fee(0, T_FEE_RATE)?, (0, 0));
        Ok(())
    }

    #[test]
    fn a_price_limit_behind_the_price_is_rejected() {
        // Selling B must raise the price: a limit below it is unreachable, in either the `==`
        // sense (handled by the loop) or the strict sense (handled up front).
        let outcome = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            T_STATE,
            T_AMOUNT_IN,
            T_SQRT_PRICE - 1,
            8,
        );
        assert!(matches!(
            outcome,
            Err(AmmError::UnreachablePriceLimit { current: T_SQRT_PRICE, limit })
            if limit == T_SQRT_PRICE - 1
        ));
        let outcome =
            swap_exact_in(&EmptySource, Direction::AtoB, T_STATE, 1_000, T_SQRT_PRICE + 1, 8);
        assert!(matches!(outcome, Err(AmmError::UnreachablePriceLimit { .. })));
    }

    #[test]
    fn a_boundary_behind_the_price_is_ignored() -> Result<(), AmmError> {
        // A stale source reporting a tick below the price must not drag the swap backwards: the
        // boundary is skipped and the quote matches the boundary-free one.
        struct Stale;
        impl TickSource for Stale {
            fn next_boundary_up(&self, _tick: i32) -> Option<Boundary> {
                Some(Boundary { tick: 0, sqrt_price: T_SQRT_PRICE - 1_000, liquidity_net: 0 })
            }
            fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
                None
            }
        }
        let stale =
            swap_exact_in(&Stale, Direction::BtoA, T_STATE, T_AMOUNT_IN, MAX_SQRT_PRICE, 8)?;
        let clean =
            swap_exact_in(&EmptySource, Direction::BtoA, T_STATE, T_AMOUNT_IN, MAX_SQRT_PRICE, 8)?;
        assert_eq!(stale.amount_out, clean.amount_out);
        assert_eq!(stale.amount_out, T_AMOUNT_OUT);
        Ok(())
    }

    #[test]
    fn degenerate_starting_state_is_rejected_up_front() {
        assert!(matches!(
            swap_exact_in(
                &EmptySource,
                Direction::BtoA,
                PoolState { sqrt_price: 0, ..T_STATE },
                1_000,
                MAX_SQRT_PRICE,
                8
            ),
            Err(AmmError::InvalidSqrtPrice(0))
        ));
        assert!(matches!(
            swap_exact_in(
                &EmptySource,
                Direction::BtoA,
                PoolState { liquidity: 0, ..T_STATE },
                1_000,
                MAX_SQRT_PRICE,
                8
            ),
            Err(AmmError::ZeroLiquidity)
        ));
    }

    #[test]
    fn crossing_a_tick_applies_the_liquidity_change() -> Result<(), AmmError> {
        // Just over the ~3.73e12 MIST it costs to drive the price up to tick 71_180, so the step
        // is cut short by the boundary rather than by the input running out.
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState {
                sqrt_price: T_SQRT_PRICE,
                liquidity: T_LIQUIDITY,
                tick: T_TICK,
                fee_rate: 0,
            },
            4_000_000_000_000,
            T_NEXT_SQRT_PRICE,
            8,
        )?;
        assert!(result.is_single_step(), "the boundary ends the swap");
        assert_eq!(result.sqrt_price_end, T_NEXT_SQRT_PRICE);
        assert_eq!(result.tick_end, T_NEXT_TICK);
        // It stopped on the boundary, so the input was not fully consumed.
        assert!(!result.input_consumed);
        Ok(())
    }

    #[test]
    fn crossing_a_tick_applies_the_liquidity_change_when_forced() -> Result<(), AmmError> {
        // With the whole range as the price limit, the swap crosses tick 71_180 and picks up its
        // liquidity, then keeps going with the new, deeper book.
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState {
                sqrt_price: T_SQRT_PRICE,
                liquidity: T_LIQUIDITY,
                tick: T_TICK,
                fee_rate: 0,
            },
            50_000_000_000_000,
            MAX_SQRT_PRICE,
            8,
        )?;
        assert_eq!(result.steps.len(), 2, "one step to the boundary, one past it");
        assert!(result.steps[0].crossed);
        assert_eq!(result.steps[0].sqrt_price_end, T_NEXT_SQRT_PRICE);
        assert_eq!(result.tick_end, T_NEXT_TICK);
        assert_eq!(result.liquidity_start, T_LIQUIDITY);
        assert_eq!(
            result.liquidity_end,
            super::apply_liquidity_change(T_LIQUIDITY, 212_759_778_363)?
        );
        assert_eq!(result.liquidity_end, T_LIQUIDITY + 212_759_778_363);
        assert!(result.sqrt_price_end > T_NEXT_SQRT_PRICE);
        assert!(result.input_consumed);
        Ok(())
    }

    #[test]
    fn empty_tick_source_is_a_single_step_over_the_whole_range() -> Result<(), AmmError> {
        let result = swap_exact_in(
            &EmptySource,
            Direction::AtoB,
            PoolState {
                sqrt_price: T_SQRT_PRICE,
                liquidity: T_LIQUIDITY,
                tick: T_TICK,
                fee_rate: 0,
            },
            1_000_000,
            MIN_SQRT_PRICE,
            8,
        )?;
        assert!(result.amount_out > 0);
        assert!(result.sqrt_price_end < T_SQRT_PRICE);
        assert_eq!(result.tick_end, T_TICK);
        Ok(())
    }

    #[test]
    fn rejects_an_absurd_number_of_crossings() {
        struct AlwaysBoundary;
        impl TickSource for AlwaysBoundary {
            fn next_boundary_up(&self, _tick: i32) -> Option<Boundary> {
                Some(Boundary { tick: MAX_TICK, sqrt_price: MAX_SQRT_PRICE, liquidity_net: 1 })
            }
            fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
                Some(Boundary { tick: MIN_TICK, sqrt_price: MIN_SQRT_PRICE, liquidity_net: 0 })
            }
        }
        // A source that never advances cannot be walked forever.
        let outcome = swap_exact_in(
            &AlwaysBoundary,
            Direction::BtoA,
            PoolState {
                sqrt_price: T_SQRT_PRICE,
                liquidity: T_LIQUIDITY,
                tick: T_TICK,
                fee_rate: 0,
            },
            u128::MAX,
            MAX_SQRT_PRICE,
            4,
        );
        assert!(outcome.is_err());
    }

    #[test]
    fn is_single_step_is_false_once_a_boundary_is_crossed() -> Result<(), AmmError> {
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            T_STATE,
            50_000_000_000_000,
            MAX_SQRT_PRICE,
            8,
        )?;
        assert_eq!(result.steps.len(), 2);
        assert!(
            !result.is_single_step(),
            "two steps is not a single step, whatever the step count says"
        );
        Ok(())
    }

    #[test]
    fn a_swap_in_the_other_direction_lowers_the_price() -> Result<(), AmmError> {
        // Selling A pushes the price down, and the amount that fits before the lower boundary is
        // reached is finite — which is only true if the direction comparison is the right way
        // round.
        let result = swap_exact_in(
            &EmptySource,
            Direction::AtoB,
            T_STATE,
            10_000_000_000,
            MIN_SQRT_PRICE,
            8,
        )?;
        assert!(result.amount_out > 0);
        assert!(result.sqrt_price_end < T_SQRT_PRICE, "A in must lower √P");
        assert!(result.is_single_step());
        Ok(())
    }

    #[test]
    fn a_price_limit_reached_exactly_ends_the_swap() -> Result<(), AmmError> {
        // Ask to move to precisely the price the pool already sits at: there is nothing to do.
        let result =
            swap_exact_in(&EmptySource, Direction::BtoA, T_STATE, 1_000_000, T_SQRT_PRICE, 8)?;
        assert_eq!(result.sqrt_price_end, T_SQRT_PRICE);
        assert_eq!(result.amount_out, 0);
        Ok(())
    }

    #[test]
    fn more_crossings_than_allowed_is_an_error() {
        // A source with an endless supply of boundaries, and a limit of one crossing.
        struct Endless;
        impl TickSource for Endless {
            fn next_boundary_up(&self, tick: i32) -> Option<Boundary> {
                Some(Boundary {
                    tick: tick + 1,
                    sqrt_price: sqrt_price_up_from(tick + 1)?,
                    liquidity_net: 1,
                })
            }
            fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
                None
            }
        }
        let outcome = swap_exact_in(
            &Endless,
            Direction::BtoA,
            T_STATE,
            50_000_000_000_000,
            MAX_SQRT_PRICE,
            2,
        );
        assert!(matches!(outcome, Err(AmmError::TooManyCrossings { limit: 2 })));
    }

    fn sqrt_price_up_from(tick: i32) -> Option<u128> {
        crate::sqrt_price_at_tick(tick).ok()
    }

    #[test]
    fn zero_liquidity_short_circuits_both_deltas() -> Result<(), AmmError> {
        assert_eq!(delta_a(T_SQRT_PRICE, T_SQRT_PRICE + 1, 0, false)?, 0);
        assert_eq!(delta_b(T_SQRT_PRICE, T_SQRT_PRICE + 1, 0, false)?, 0);
        Ok(())
    }

    #[test]
    fn an_empty_price_range_short_circuits_both_deltas() -> Result<(), AmmError> {
        assert_eq!(delta_a(T_SQRT_PRICE, T_SQRT_PRICE, 1_000, false)?, 0);
        assert_eq!(delta_b(T_SQRT_PRICE, T_SQRT_PRICE, 1_000, false)?, 0);
        Ok(())
    }

    #[test]
    fn a_limit_inside_the_boundary_stops_at_the_limit() -> Result<(), AmmError> {
        // ponytail: regression for limit-bypass; the boundary lies beyond the limit.
        let mid = T_SQRT_PRICE + (T_NEXT_SQRT_PRICE - T_SQRT_PRICE) / 2;
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            mid,
            8,
        )?;
        assert_eq!(result.sqrt_price_end, mid);
        assert!(!result.input_consumed);
        Ok(())
    }

    #[test]
    fn a_limit_inside_the_boundary_stops_at_the_limit_down() -> Result<(), AmmError> {
        // ponytail: downward mirror of the clamp above.
        let lower = LowerBoundary {
            tick: 71_150,
            sqrt_price: sqrt_price_at_tick(71_150)?,
            liquidity_net: 1_000_000,
        };
        let mid = u128::midpoint(lower.sqrt_price, T_SQRT_PRICE);
        assert!(mid < T_SQRT_PRICE && mid > lower.sqrt_price);
        let result = swap_exact_in(
            &lower,
            Direction::AtoB,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            mid,
            8,
        )?;
        assert_eq!(result.sqrt_price_end, mid);
        assert!(!result.input_consumed);
        Ok(())
    }

    #[test]
    fn stopping_at_the_price_limit_does_not_cross_the_boundary_beyond_it() -> Result<(), AmmError> {
        // The limit sits halfway to the only boundary, so the price stops short of the tick. Its
        // `liquidity_net` belongs to a tick the price never reached: applying it would move both
        // the reported liquidity and the reported tick to the far side of a boundary that was not
        // crossed.
        let mid = T_SQRT_PRICE + (T_NEXT_SQRT_PRICE - T_SQRT_PRICE) / 2;
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            mid,
            8,
        )?;
        assert_eq!(result.sqrt_price_end, mid);
        assert_eq!(result.liquidity_end, T_LIQUIDITY, "the boundary's liquidity is not in force");
        assert_eq!(result.tick_end, T_TICK, "no tick was crossed");
        assert!(result.is_single_step());
        Ok(())
    }

    #[test]
    fn a_boundary_beyond_the_price_limit_is_not_counted_as_a_crossing() -> Result<(), AmmError> {
        // With the same truncated swap, not one boundary was crossed — so a caller that allows no
        // crossings at all must get a quote, not `TooManyCrossings`.
        let mid = T_SQRT_PRICE + (T_NEXT_SQRT_PRICE - T_SQRT_PRICE) / 2;
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            mid,
            0,
        )?;
        assert_eq!(result.sqrt_price_end, mid);
        Ok(())
    }

    #[test]
    fn one_unit_of_price_move_is_worth_less_than_one_base_unit_of_output() -> Result<(), AmmError> {
        // T's step is `ΔS = floor(in·2^64 / L)`. Rounding it up instead moves the price by one
        // unit, which at this step width buys less than one base unit of USDC — so the reproduction
        // does not rest on which side of the floor the chain took.
        let delta = next_sqrt_price_up(T_SQRT_PRICE, T_LIQUIDITY, 99_950_000_000)? - T_SQRT_PRICE;
        let reached = T_SQRT_PRICE + delta;
        assert_eq!(delta_a(reached, reached + 1, T_LIQUIDITY, false)?, 0);
        Ok(())
    }

    #[test]
    fn truncated_swaps_charge_fee_only_on_consumed_input() -> Result<(), AmmError> {
        // ponytail: non-zero fee plus a limit that truncates must not charge the full fee.
        let mid = T_SQRT_PRICE + (T_NEXT_SQRT_PRICE - T_SQRT_PRICE) / 2;
        let full_fee = take_fee(50_000_000_000_000, T_FEE_RATE)?.1;
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: T_FEE_RATE, ..T_STATE },
            50_000_000_000_000,
            mid,
            8,
        )?;
        assert!(!result.input_consumed);
        assert!(result.fee < full_fee, "truncated fee {} < full fee {full_fee}", result.fee);
        assert!(result.fee > 0);
        Ok(())
    }

    /// `delta_a` rejects a zero price *before* it looks at liquidity, so an all-zero range is an
    /// error rather than a zero output. This pins the order of the two guards, which is otherwise
    /// unobservable: once the zero-price guard is in front, `liquidity == 0 || low == high` and
    /// `liquidity == 0 && low == high` compute the same thing, so that particular mutation is
    /// semantically equivalent and cannot be killed by any test. The same holds for `delta_b`,
    /// whose two guards both short-circuit to the same zero without any ordering subtlety.
    #[test]
    fn the_zero_price_guard_precedes_the_liquidity_guard() -> Result<(), AmmError> {
        assert!(matches!(delta_a(0, 0, 1_000, false), Err(AmmError::InvalidSqrtPrice(0))));
        assert_eq!(delta_b(0, 0, 1_000, false)?, 0, "delta_b has no price guard");
        Ok(())
    }

    #[test]
    fn a_zero_price_is_rejected() {
        assert!(matches!(
            delta_a(0, T_SQRT_PRICE, 1_000, false),
            Err(AmmError::InvalidSqrtPrice(0))
        ));
    }

    #[test]
    fn zero_input_is_a_no_op_not_an_error() -> Result<(), AmmError> {
        // `remaining` starts at zero, so the loop must not run at all. A `>=` in the loop
        // condition would step with an empty input, reach no new price, and fail with
        // `NoProgress` instead of returning the untouched state.
        let result =
            swap_exact_in(&SingleBoundary, Direction::BtoA, T_STATE, 0, MAX_SQRT_PRICE, 8)?;
        assert_eq!(result.amount_out, 0);
        assert_eq!(result.fee, 0);
        assert_eq!(result.steps.len(), 0);
        assert_eq!(result.sqrt_price_end, T_SQRT_PRICE);
        assert!(result.input_consumed);
        Ok(())
    }

    /// A source whose only boundary sits exactly at the pool's current price.
    struct BoundaryAtPrice {
        direction: Direction,
    }

    impl TickSource for BoundaryAtPrice {
        fn next_boundary_up(&self, _tick: i32) -> Option<Boundary> {
            (self.direction == Direction::BtoA).then_some(Boundary {
                tick: T_TICK,
                sqrt_price: T_SQRT_PRICE,
                liquidity_net: 1_000,
            })
        }

        fn next_boundary_down(&self, _tick: i32) -> Option<Boundary> {
            (self.direction == Direction::AtoB).then_some(Boundary {
                tick: T_TICK,
                sqrt_price: T_SQRT_PRICE,
                liquidity_net: 1_000,
            })
        }
    }

    #[test]
    fn a_boundary_exactly_at_the_price_is_behind_not_ahead() -> Result<(), AmmError> {
        // A pool can sit exactly on an initialised tick. That tick's liquidity is already in force,
        // so a source handing it back must not be read as a boundary ahead: stepping to it would be
        // no progress at all. Both directions must quote exactly what an empty tick set quotes.
        for direction in [Direction::BtoA, Direction::AtoB] {
            let limit = match direction {
                Direction::BtoA => MAX_SQRT_PRICE,
                Direction::AtoB => MIN_SQRT_PRICE,
            };
            let expected = swap_exact_in(&EmptySource, direction, T_STATE, T_AMOUNT_IN, limit, 8)?;
            let result = swap_exact_in(
                &BoundaryAtPrice { direction },
                direction,
                T_STATE,
                T_AMOUNT_IN,
                limit,
                8,
            )?;
            assert_eq!(result.amount_out, expected.amount_out);
            assert_eq!(result.sqrt_price_end, expected.sqrt_price_end);
            assert_eq!(
                result.liquidity_end, expected.liquidity_end,
                "the tick at the price is already in force"
            );
        }
        Ok(())
    }

    #[test]
    fn a_single_allowed_crossing_is_not_an_error() -> Result<(), AmmError> {
        // Exactly one boundary lies ahead, and exactly one crossing is allowed. A `>=` in the
        // crossing guard would reject the first boundary instead of the second.
        let result = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            MAX_SQRT_PRICE,
            1,
        )?;
        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.tick_end, T_NEXT_TICK);
        Ok(())
    }

    #[test]
    fn no_allowed_crossing_rejects_the_first_boundary() {
        // With zero crossings allowed, reaching any boundary is an error. An `==` in the guard
        // would never fire here (`1 == 0` is false) and let the swap through.
        let outcome = swap_exact_in(
            &SingleBoundary,
            Direction::BtoA,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            MAX_SQRT_PRICE,
            0,
        );
        assert!(matches!(outcome, Err(AmmError::TooManyCrossings { limit: 0 })));
    }

    /// A boundary below the price, for the sell-A direction. Tick 71_150 sits on the pool's
    /// spacing grid, and crossing it downwards removes liquidity.
    struct LowerBoundary {
        tick: i32,
        sqrt_price: u128,
        liquidity_net: i128,
    }

    impl TickSource for LowerBoundary {
        fn next_boundary_up(&self, _tick: i32) -> Option<Boundary> {
            None
        }

        fn next_boundary_down(&self, tick: i32) -> Option<Boundary> {
            (tick > self.tick).then_some(Boundary {
                tick: self.tick,
                sqrt_price: self.sqrt_price,
                liquidity_net: self.liquidity_net,
            })
        }
    }

    #[test]
    fn selling_a_crosses_a_lower_boundary() -> Result<(), AmmError> {
        let lower = LowerBoundary {
            tick: 71_150,
            sqrt_price: sqrt_price_at_tick(71_150)?,
            // ponytail: lower-tick nets are positive upwards; crossing down removes it.
            liquidity_net: 1_000_000,
        };
        assert!(lower.sqrt_price < T_SQRT_PRICE, "the boundary must lie below the price");
        let result = swap_exact_in(
            &lower,
            Direction::AtoB,
            PoolState { fee_rate: 0, ..T_STATE },
            50_000_000_000_000,
            MIN_SQRT_PRICE,
            8,
        )?;
        assert!(result.steps.len() >= 2, "the boundary must cut the swap");
        assert!(result.steps[0].crossed);
        assert_eq!(result.steps[0].sqrt_price_end, lower.sqrt_price);
        assert_eq!(result.tick_end, 71_150);
        assert_eq!(result.liquidity_end, T_LIQUIDITY - 1_000_000);
        assert!(result.sqrt_price_end < T_SQRT_PRICE);
        assert!(result.amount_out > 0);
        Ok(())
    }
}
