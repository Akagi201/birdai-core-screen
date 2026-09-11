//! Protocol numeric newtypes.
//!
//! Sui's `MoveTypeLayout` describes these as plain unsigned integers because that is how they are
//! serialised, but the on-chain types are signed. Keeping the reinterpretation in one place stops
//! sign handling from leaking into venue code.

use std::fmt;

use move_core_types::annotated_visitor::StructDriver;

use crate::{
    error::DecodeError,
    visitor::{StructDecoder, bool_field, u32_field, u64_field, u128_field},
};

/// `0x714a63a0…::i32::I32` — a `u32` holding a two's-complement `i32`.
///
/// Cetus stores tick indices in this type; the layout exposes only `bits: u32`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct I32(i32);

impl I32 {
    /// Wrap a native `i32`.
    #[must_use]
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    /// Reinterpret serialised two's-complement bits.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits.cast_signed())
    }

    /// The signed value.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// The serialised two's-complement bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0 as u32
    }
}

impl From<i32> for I32 {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl From<I32> for i32 {
    fn from(value: I32) -> Self {
        value.0
    }
}

impl fmt::Display for I32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// `0x714a63a0…::i128::I128` — a `u128` holding a two's-complement `i128`.
///
/// Used for `Tick::liquidity_net`, which is negative for ticks above the current price.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct I128(i128);

impl I128 {
    /// Wrap a native `i128`.
    #[must_use]
    pub const fn new(value: i128) -> Self {
        Self(value)
    }

    /// Reinterpret serialised two's-complement bits.
    #[must_use]
    pub const fn from_bits(bits: u128) -> Self {
        Self(bits.cast_signed())
    }

    /// The signed value.
    #[must_use]
    pub const fn get(self) -> i128 {
        self.0
    }

    /// The serialised two's-complement bits.
    #[must_use]
    pub const fn bits(self) -> u128 {
        self.0 as u128
    }
}

impl From<i128> for I128 {
    fn from(value: i128) -> Self {
        Self(value)
    }
}

impl From<I128> for i128 {
    fn from(value: I128) -> Self {
        value.0
    }
}

impl fmt::Display for I128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// `0xbe21a061…::option_u64::OptionU64` — Cetus's *custom* option.
///
/// This is **not** `0x1::option::Option`. The standard option is a one-element-or-empty
/// `vector<T>`, so it costs one length byte for `None`; `OptionU64` is a struct with a `bool` and
/// a `u64`, so it always costs nine bytes and the payload is present even when `is_none` is true.
/// A decoder that recognises options by name gets this wrong; a layout-driven one cannot, because
/// the layout says `struct`, not `vector`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OptionU64 {
    /// True when the option is empty. The `value` field is still serialised.
    pub is_none: bool,
    /// The payload, meaningful only when `is_none` is false.
    pub value: u64,
}

impl OptionU64 {
    /// Convert to a native `Option`, discarding the payload when `is_none`.
    #[must_use]
    pub const fn to_option(self) -> Option<u64> {
        if self.is_none { None } else { Some(self.value) }
    }
}

/// Decoder for `0x714a63a0…::i32::I32 { bits: u32 }`.
#[derive(Debug, Default, Clone, Copy)]
pub struct I32Decoder;

impl<'b, 'l> StructDecoder<'b, 'l> for I32Decoder {
    type Output = I32;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut bits = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bits" => bits = Some(u32_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        bits.map(I32::from_bits).ok_or(DecodeError::MissingField("bits"))
    }
}

/// Decoder for `0x714a63a0…::i128::I128 { bits: u128 }`.
#[derive(Debug, Default, Clone, Copy)]
pub struct I128Decoder;

impl<'b, 'l> StructDecoder<'b, 'l> for I128Decoder {
    type Output = I128;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut bits = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bits" => bits = Some(u128_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        bits.map(I128::from_bits).ok_or(DecodeError::MissingField("bits"))
    }
}

/// Decoder for `0xbe21a061…::option_u64::OptionU64 { is_none: bool, v: u64 }`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OptionU64Decoder;

impl<'b, 'l> StructDecoder<'b, 'l> for OptionU64Decoder {
    type Output = OptionU64;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut is_none = None;
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "is_none" => is_none = Some(bool_field(driver)?),
                "v" => value = Some(u64_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        Ok(OptionU64 {
            is_none: is_none.ok_or(DecodeError::MissingField("is_none"))?,
            // A missing payload is a missing field, not a zero: `Some(0)` out of thin air on a
            // pricing path would be a silent wrong answer, and the fail-closed direction is to
            // reject the node.
            value: value.ok_or(DecodeError::MissingField("v"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{I32, I128, OptionU64};

    #[test]
    fn i32_round_trips_through_two_complement_bits() {
        assert_eq!(I32::from_bits(71_162).get(), 71_162);
        // -443636 is Cetus's MIN_TICK; as 32-bit bits it is 2^32 - 443636.
        assert_eq!(I32::from_bits(443_636_u32.wrapping_neg()).get(), -443_636);
        assert_eq!(I32::new(-1).bits(), u32::MAX);
    }

    #[test]
    fn i128_interprets_large_liquidity_net_as_negative() {
        // Observed on mainnet: a `liquidity_net` whose `bits` are close to 2^128 is negative.
        let bits: u128 = 340_282_366_920_938_463_463_374_605_480_910_926_342;
        assert!(I128::from_bits(bits).get() < 0);
        assert_eq!(I128::new(-1).bits(), u128::MAX);
    }

    #[test]
    fn option_u64_keeps_the_payload_when_empty() {
        let empty = OptionU64 { is_none: true, value: 0 };
        assert_eq!(empty.to_option(), None);
        // The payload is serialised even when `is_none`, which is what distinguishes this from
        // `0x1::option::Option`.
        let stale = OptionU64 { is_none: true, value: 42 };
        assert_eq!(stale.to_option(), None);
        assert_eq!(OptionU64 { is_none: false, value: 42 }.to_option(), Some(42));
    }

    #[test]
    fn option_u64_decodes_against_a_real_layout() -> Result<(), crate::error::DecodeError> {
        use move_core_types::{
            account_address::AccountAddress,
            annotated_value::{MoveFieldLayout, MoveStructLayout, MoveTypeLayout},
            identifier::Identifier,
            language_storage::StructTag,
        };

        use super::OptionU64Decoder;
        use crate::{error::DecodeError, visitor::decode_struct};

        // Test-only identifiers are valid Move identifiers by construction.
        fn ident(text: &str) -> Identifier {
            Identifier::new(text).unwrap_or_else(|_| unreachable!())
        }

        fn layout(fields: Vec<(&str, MoveTypeLayout)>) -> MoveStructLayout {
            MoveStructLayout {
                type_: StructTag {
                    address: AccountAddress::TWO,
                    module: ident("option_u64"),
                    name: ident("OptionU64"),
                    type_params: vec![],
                },
                fields: fields
                    .into_iter()
                    .map(|(name, layout)| MoveFieldLayout { name: ident(name), layout })
                    .collect(),
            }
        }

        let full = layout(vec![("is_none", MoveTypeLayout::Bool), ("v", MoveTypeLayout::U64)]);
        // `is_none = false`, `v = 515_646` (`0x07DE3E`, little-endian below).
        let bytes = [0x00, 0x3e, 0xde, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00];
        let decoded = decode_struct(&bytes, &full, OptionU64Decoder)?;
        assert_eq!(decoded.to_option(), Some(515_646));

        // A layout without `v` — an older or foreign `OptionU64` — is a missing field, not a
        // zero. `Some(0)` out of thin air on a pricing path would be a silent wrong answer.
        let without_payload = layout(vec![("is_none", MoveTypeLayout::Bool)]);
        assert!(matches!(
            decode_struct(&[0x00], &without_payload, OptionU64Decoder),
            Err(DecodeError::MissingField("v"))
        ));
        Ok(())
    }
}
