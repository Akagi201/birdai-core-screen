//! A `U256` that refuses to wrap.
//!
//! `move_core_types::u256::U256` is the right width for concentrated-liquidity math — a 128×128
//! product needs 256 bits and nothing here needs more (see [`crate`] for the derivation) — but its
//! operator impls are **wrapping**: its own documentation says `Add`/`Sub`/`Mul` "ignore
//! overflows", and `Div`/`Rem` panic on a zero divisor. Wrapping in an integer pricing path is a
//! silent wrong answer, so arithmetic goes through this newtype, which only exposes checked
//! operations and converts failures into [`AmmError`].

use move_core_types::u256::U256;

use crate::error::AmmError;

/// An unsigned 256-bit integer whose arithmetic cannot wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct CheckedU256(U256);

impl CheckedU256 {
    /// Zero.
    #[must_use]
    #[inline]
    pub const fn zero() -> Self {
        Self(U256::zero())
    }

    /// One.
    #[must_use]
    #[inline]
    pub const fn one() -> Self {
        Self(U256::one())
    }

    /// The largest representable value, `2^256 - 1`.
    #[must_use]
    #[inline]
    pub const fn max_value() -> Self {
        Self(U256::max_value())
    }

    /// Widen a `u128`.
    #[must_use]
    #[inline]
    pub fn from_u128(value: u128) -> Self {
        Self(U256::from(value))
    }

    /// Widen a `u64`.
    #[must_use]
    #[inline]
    pub fn from_u64(value: u64) -> Self {
        Self(U256::from(value))
    }

    /// The underlying `U256`.
    #[must_use]
    #[inline]
    pub const fn inner(self) -> U256 {
        self.0
    }

    /// True when the value is zero.
    #[must_use]
    #[inline]
    pub fn is_zero(self) -> bool {
        self.0 == U256::zero()
    }

    /// Narrow to a `u128`, failing rather than truncating.
    #[inline]
    pub fn to_u128(self) -> Result<u128, AmmError> {
        u128::try_from(self.0).map_err(|_| AmmError::Overflow { op: "narrow to u128" })
    }

    /// Checked addition.
    #[inline]
    pub fn checked_add(self, rhs: Self) -> Result<Self, AmmError> {
        self.0.checked_add(rhs.0).map(Self).ok_or(AmmError::Overflow { op: "add" })
    }

    /// Checked subtraction.
    #[inline]
    pub fn checked_sub(self, rhs: Self) -> Result<Self, AmmError> {
        self.0.checked_sub(rhs.0).map(Self).ok_or(AmmError::Overflow { op: "sub" })
    }

    /// Checked multiplication.
    #[inline]
    pub fn checked_mul(self, rhs: Self) -> Result<Self, AmmError> {
        self.0.checked_mul(rhs.0).map(Self).ok_or(AmmError::Overflow { op: "mul" })
    }

    /// Checked division, rounding down.
    #[inline]
    pub fn checked_div(self, rhs: Self) -> Result<Self, AmmError> {
        if rhs.is_zero() {
            return Err(AmmError::DivByZero { op: "div" });
        }
        self.0.checked_div(rhs.0).map(Self).ok_or(AmmError::DivByZero { op: "div" })
    }

    /// Checked division, rounding up.
    #[inline]
    pub fn checked_div_ceil(self, rhs: Self) -> Result<Self, AmmError> {
        let (quotient, remainder) = self.checked_div_rem(rhs)?;
        if remainder.is_zero() { Ok(quotient) } else { quotient.checked_add(Self::one()) }
    }

    /// Checked division returning quotient and remainder.
    #[inline]
    pub fn checked_div_rem(self, rhs: Self) -> Result<(Self, Self), AmmError> {
        if rhs.is_zero() {
            return Err(AmmError::DivByZero { op: "div_rem" });
        }
        let quotient = self.0.checked_div(rhs.0).ok_or(AmmError::DivByZero { op: "div_rem" })?;
        let remainder = self.0.checked_rem(rhs.0).ok_or(AmmError::DivByZero { op: "div_rem" })?;
        Ok((Self(quotient), Self(remainder)))
    }

    /// Checked left shift.
    #[inline]
    pub fn checked_shl(self, bits: u32) -> Result<Self, AmmError> {
        self.0.checked_shl(bits).map(Self).ok_or(AmmError::Overflow { op: "shl" })
    }

    /// Checked right shift.
    #[inline]
    pub fn checked_shr(self, bits: u32) -> Result<Self, AmmError> {
        self.0.checked_shr(bits).map(Self).ok_or(AmmError::Overflow { op: "shr" })
    }
}

#[cfg(test)]
mod tests {
    use super::CheckedU256;
    use crate::error::AmmError;

    #[test]
    fn arithmetic_does_not_wrap() {
        let max = CheckedU256::max_value();
        assert!(matches!(max.checked_add(CheckedU256::one()), Err(AmmError::Overflow { .. })));
        assert!(matches!(
            max.checked_mul(CheckedU256::from_u64(2)),
            Err(AmmError::Overflow { .. })
        ));
        assert!(matches!(
            CheckedU256::zero().checked_sub(CheckedU256::one()),
            Err(AmmError::Overflow { .. })
        ));
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        assert!(matches!(
            CheckedU256::one().checked_div(CheckedU256::zero()),
            Err(AmmError::DivByZero { .. })
        ));
        assert!(matches!(
            CheckedU256::one().checked_div_rem(CheckedU256::zero()),
            Err(AmmError::DivByZero { .. })
        ));
    }

    #[test]
    fn div_ceil_rounds_only_when_there_is_a_remainder() -> Result<(), AmmError> {
        let seven = CheckedU256::from_u64(7);
        let two = CheckedU256::from_u64(2);
        assert_eq!(seven.checked_div_ceil(two)?, CheckedU256::from_u64(4));
        assert_eq!(seven.checked_div(two)?, CheckedU256::from_u64(3));
        assert_eq!(seven.checked_div_rem(two)?.1, CheckedU256::from_u64(1));
        let eight = CheckedU256::from_u64(8);
        assert_eq!(eight.checked_div_ceil(two)?, CheckedU256::from_u64(4));
        Ok(())
    }

    #[test]
    fn to_u128_refuses_to_truncate() -> Result<(), AmmError> {
        let too_big = CheckedU256::one().checked_shl(128)?;
        assert!(matches!(too_big.to_u128(), Err(AmmError::Overflow { .. })));
        let fits = CheckedU256::from_u128(u128::MAX);
        assert_eq!(fits.to_u128()?, u128::MAX);
        Ok(())
    }
}
