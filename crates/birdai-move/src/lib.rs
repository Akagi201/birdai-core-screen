//! Move value plumbing: protocol newtypes, layout-driven decoders and annotated dumps.
//!
//! The crate deliberately does **not** reimplement BCS or layout handling. Those live in
//! `move-core-types::annotated_visitor`, which already provides a single-pass, layout-driven
//! decoder with byte offsets. What this crate adds is the ergonomic layer on top:
//!
//! * [`StructDecoder`] — write a venue struct as a `match` over field names, so a package upgrade
//!   that appends or reorders fields needs no code change;
//! * [`Dump`] — the same walk, but producing an annotated tree with exact BCS byte ranges;
//! * [`numeric`] — `I32`, `I128` and Cetus's non-standard `OptionU64`;
//! * [`tag`] — `StructTag` predicates and abbreviations.
//!
//! # Example
//!
//! ```ignore
//! struct BalanceDecoder;
//!
//! impl<'b, 'l> StructDecoder<'b, 'l> for BalanceDecoder {
//!     type Output = u64;
//!
//!     fn decode(&mut self, driver: &mut StructDriver<'_, 'b, 'l>) -> Result<u64, DecodeError> {
//!         let mut value = None;
//!         while let Some(field) = driver.peek_field() {
//!             match field.name.as_str() {
//!                 "value" => value = Some(u64_field(driver)?),
//!                 _ => { driver.skip_field()?; }
//!             }
//!         }
//!         required(value, "value")
//!     }
//! }
//! ```

pub mod dump;
pub mod error;
pub mod numeric;
pub mod tag;
pub mod visitor;

pub use dump::{Dump, DumpField, DumpItem, DumpOptions, DumpValue, Span, short_type_of};
pub use error::DecodeError;
pub use numeric::{I32, I32Decoder, I128, I128Decoder, OptionU64, OptionU64Decoder};
pub use tag::{
    MOVE_STDLIB, SUI_FRAMEWORK, balance_inner, coin_inner, full_address, is_cetus_skip_list,
    is_child_container, is_tag, short_address, short_tag, short_type,
};
pub use visitor::{
    AddressV, BoolV, BytesV, StructDecoder, StructVisitor, U8V, U16V, U32V, U64V, U128V, U256V,
    VecVisitor, address_field, address_vec_field, bool_field, bytes_field, decode_struct,
    decode_value, required, struct_field, struct_vec_field, u8_field, u16_field, u32_field,
    u64_field, u64_vec_field, u128_field, u128_vec_field, u256_field,
};
