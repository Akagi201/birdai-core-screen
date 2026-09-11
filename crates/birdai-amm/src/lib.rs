//! Concentrated-liquidity integer math: tick ↔ price conversion and exact-input swap arithmetic.
//!
//! This crate is the one place the exercise asks us to write rather than reuse: the Sui crates
//! contain no AMM math. It has **no I/O and no dependency on the venue or state crates** — it takes
//! numbers and returns numbers, which is what makes it testable in microseconds and property-tested
//! against an independent reference.
//!
//! # Why 256 bits is exactly enough
//!
//! rev 1 of the design assumed 512-bit intermediates were needed. That is wrong, and the bound is
//! provable from the pool's own invariants:
//!
//! * Each of the pool's reserves is a `0x2::balance::Balance<T>`, i.e. a `u64`, so the **virtual**
//!   reserves are bounded by total supply: `a_v, b_v < 2^64`.
//! * The pool maintains `L = √(a_v · b_v)` for the active range, so **`L < 2^64`**.
//! * The output is `⌊(L ≪ 64) · ΔS / (S · S')⌋` with `ΔS = ⌊amount_in · 2^64 / L⌋ < 2^128`. Hence
//!   `(L ≪ 64) · ΔS < 2^128 · 2^128 = 2^256`, which fits `U256` exactly.
//! * The denominator `S · S'` has `S, S' < 2^128`, so it is also `< 2^256`.
//!
//! Every intermediate therefore fits in [`move_core_types::u256::U256`], and no wider type is
//! needed. Arithmetic goes through [`CheckedU256`], because `U256`'s own operators wrap.
//!
//! # Rounding
//!
//! All directions favour the pool: fees floor, the price an input reaches floors, outputs floor,
//! and amounts the pool is owed ceil. [`swap`] documents the transaction that pins this down.
//!
//! # Mainnet anchor
//!
//! ```
//! # use birdai_amm::{Direction, PoolState, SwapResult, swap, swap::TickSource, swap::Boundary,
//! #                 tick::MAX_SQRT_PRICE};
//! # struct None_;
//! # impl TickSource for None_ {
//! #     fn next_boundary_up(&self, _: i32) -> Option<Boundary> { None }
//! #     fn next_boundary_down(&self, _: i32) -> Option<Boundary> { None }
//! # }
//! // Pool A of the exercise, at the version transaction T consumed.
//! let state = PoolState {
//!     sqrt_price: 647_308_812_393_509_050_120, // Q64.64
//!     liquidity: 120_115_891_674_982,
//!     tick: 71_162,
//!     fee_rate: 500, // 5 bps
//! };
//! let result: SwapResult = swap::swap_exact_in(
//!     &None_,           // no initialised ticks in reach; the move is < 1 tick
//!     Direction::BtoA,  // SUI in, USDC out
//!     state,
//!     100_000_000_000,  // 100 SUI in MIST
//!     MAX_SQRT_PRICE,
//!     8,
//! )
//! .expect("well-formed pool");
//! assert_eq!(result.amount_out, 81_168_759); // the on-chain result, exactly
//! ```

pub mod checked;
pub mod error;
pub mod swap;
pub mod tick;

pub use checked::CheckedU256;
pub use error::AmmError;
pub use swap::{
    Boundary, Direction, FEE_DENOMINATOR, PoolState, Step, SwapResult, TickSource,
    apply_liquidity_change, delta_a, delta_b, next_sqrt_price_down, next_sqrt_price_up,
    swap_exact_in, take_fee,
};
pub use tick::{
    MAX_SQRT_PRICE, MAX_TICK, MIN_SQRT_PRICE, MIN_TICK, Q64, TICK_FACTORS,
    TICK_PRICE_TOLERANCE_BITS, sqrt_price_at_tick, tick_at_sqrt_price, tick_price_tolerance,
};
