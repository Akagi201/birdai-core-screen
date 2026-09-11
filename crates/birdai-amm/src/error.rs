//! Arithmetic errors.
//!
//! Concentrated-liquidity math is all integer arithmetic on bounded types. Every failure mode is
//! explicit here because the alternative — a silent wrap — is a wrong price, which is the one
//! outcome a market-making system must never produce.

use thiserror::Error;

/// Something went wrong while evaluating pool arithmetic.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AmmError {
    /// An intermediate value did not fit in the type the algorithm uses.
    ///
    /// The bounds are chosen so that this cannot happen for a well-formed pool (see the crate
    /// documentation for the derivation); it is a hard error rather than a wrap so that a
    /// malformed input is loud.
    #[error("arithmetic overflow while {op}")]
    Overflow {
        /// The operation that overflowed.
        op: &'static str,
    },

    /// A divisor was zero.
    #[error("division by zero while {op}")]
    DivByZero {
        /// The operation that divided by zero.
        op: &'static str,
    },

    /// The pool's fee rate is not below its denominator, so no fee can be computed from it.
    #[error("fee rate {rate} is not below the denominator")]
    InvalidFeeRate {
        /// The offending fee rate.
        rate: u64,
    },

    /// The pool has no active liquidity, so no trade can be priced.
    #[error("pool has zero active liquidity")]
    ZeroLiquidity,

    /// The pool's stored square-root price is zero, which is not a valid Q64.64 price.
    #[error("invalid square-root price {0}")]
    InvalidSqrtPrice(u128),

    /// A tick index fell outside the range representable by the pool.
    #[error("tick {tick} is outside [{min}, {max}]")]
    InvalidTick {
        /// The offending tick.
        tick: i32,
        /// Minimum supported tick.
        min: i32,
        /// Maximum supported tick.
        max: i32,
    },

    /// The price limit for a swap was on the wrong side of the current price.
    #[error("price limit {limit} is not reachable from {current} while trading in this direction")]
    UnreachablePriceLimit {
        /// Current square-root price.
        current: u128,
        /// Requested limit.
        limit: u128,
    },

    /// A swap consumed no input, which would otherwise spin forever.
    #[error("swap step made no progress with {remaining} of input remaining")]
    NoProgress {
        /// Input still unconsumed when the step stalled.
        remaining: u128,
    },

    /// The swap crossed more tick boundaries than the configured safety limit.
    #[error("swap crossed more than {limit} tick boundaries")]
    TooManyCrossings {
        /// The configured limit.
        limit: u32,
    },
}
