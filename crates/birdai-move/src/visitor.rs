//! A small toolkit for turning BCS bytes plus a resolved layout into typed Rust values.
//!
//! Everything here is a thin layer over [`move_core_types::annotated_visitor`]. The framework
//! already gives us the two things that matter:
//!
//! * a **single pass** over the bytes — no intermediate `MoveValue` tree, and
//! * **byte offsets** (`ValueDriver::start` / `position`) for every value.
//!
//! What the toolkit adds is ergonomics: a [`StructDecoder`] trait so a venue struct can be written
//! as a `match` over field names, plus field helpers that keep decoding code free of visitor
//! boilerplate.

use move_core_types::{
    account_address::AccountAddress,
    annotated_value::{MoveStruct, MoveStructLayout, MoveTypeLayout, MoveValue},
    annotated_visitor::{StructDriver, ValueDriver, VariantDriver, VecDriver, Visitor},
    u256::U256,
    visitor_default,
};

use crate::error::DecodeError;

/// A decoder for one Move struct.
///
/// Implementors are stateful only in so far as they accumulate fields; the `&mut self` receiver
/// exists so a decoder can carry budgets or scratch buffers.
///
/// Decoders match fields **by name**, which is what makes them survive a package upgrade that
/// appends or reorders fields. A field the decoder requires but the layout does not have is a hard
/// [`DecodeError::MissingField`] — silence would mean pricing against a stale schema.
pub trait StructDecoder<'b, 'l> {
    /// The Rust value produced.
    type Output;

    /// Read the fields of the current struct from `driver`.
    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError>;
}

/// Adapts a [`StructDecoder`] into a [`Visitor`], so it can be used for a nested field or as the
/// entry point for a whole object.
#[derive(Debug, Default, Clone, Copy)]
pub struct StructVisitor<D>(pub D);

impl<'b, 'l, D: StructDecoder<'b, 'l>> Visitor<'b, 'l> for StructVisitor<D> {
    type Value = D::Output;
    type Error = DecodeError;

    fn visit_struct(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        self.0.decode(driver)
    }

    visitor_default! { <'b, 'l> u8, u16, u32, u64, u128, u256, bool, address, signer, vector, variant = Err(DecodeError::UnexpectedType { expected: "a struct" }) }
}

/// A visitor that collects a vector's elements using an inner visitor.
#[derive(Debug, Default, Clone, Copy)]
pub struct VecVisitor<V>(pub V);

/// Upper bound on the elements pre-allocated from a vector's length prefix.
///
/// The length is untrusted BCS input: a corrupt `u64::MAX` prefix must not turn into a huge
/// allocation. The walk still reads every element the driver yields; only the up-front
/// reservation is capped.
const MAX_VECTOR_PREALLOC: usize = 1_024;

impl<'b, 'l, V> Visitor<'b, 'l> for VecVisitor<V>
where
    V: Visitor<'b, 'l, Error = DecodeError>,
{
    type Value = Vec<V::Value>;
    type Error = DecodeError;

    fn visit_vector(
        &mut self,
        driver: &mut VecDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        let prealloc = usize::try_from(driver.len()).unwrap_or(0).min(MAX_VECTOR_PREALLOC);
        let mut items = Vec::with_capacity(prealloc);
        while let Some(item) = driver.next_element(&mut self.0)? {
            items.push(item);
        }
        Ok(items)
    }

    visitor_default! { <'b, 'l> u8, u16, u32, u64, u128, u256, bool, address, signer, struct, variant = Err(DecodeError::UnexpectedType { expected: "a vector" }) }
}

/// A visitor that borrows a `vector<u8>` directly out of the input bytes.
///
/// This is the zero-copy path for `string::String`, `ascii::String`, `TypeName` and raw byte
/// vectors: the framework has already read the length prefix by the time `visit_vector` runs, so
/// the bytes can be sliced without visiting a single element.
#[derive(Debug, Default, Clone, Copy)]
pub struct BytesV;

impl<'b, 'l> Visitor<'b, 'l> for BytesV {
    type Value = &'b [u8];
    type Error = DecodeError;

    fn visit_vector(
        &mut self,
        driver: &mut VecDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        if !matches!(driver.element_layout(), MoveTypeLayout::U8) {
            return Err(DecodeError::UnexpectedType { expected: "vector<u8>" });
        }
        let start = driver.position();
        let len = usize::try_from(driver.len()).unwrap_or(usize::MAX);
        let end = start.checked_add(len).ok_or(DecodeError::UnexpectedEnd)?;
        let bytes = driver.bytes().get(start..end).ok_or(DecodeError::UnexpectedEnd)?;
        while driver.skip_element()? {}
        Ok(bytes)
    }

    visitor_default! { <'b, 'l> u8, u16, u32, u64, u128, u256, bool, address, signer, struct, variant = Err(DecodeError::UnexpectedType { expected: "vector<u8>" }) }
}

/// Generates a visitor that accepts exactly one scalar type.
///
/// The list of methods routed to the error default is passed explicitly, because `visitor_default!`
/// cannot subtract the one method the macro implements itself.
macro_rules! leaf_visitor {
    ($name:ident, $method:ident, $ty:ty, $label:literal, $($defaulted:ident),* $(,)?) => {
        #[doc = concat!("A visitor that accepts exactly one `", $label, "`.")]
        #[derive(Debug, Default, Clone, Copy)]
        pub struct $name;

        impl<'b, 'l> Visitor<'b, 'l> for $name {
            type Value = $ty;
            type Error = DecodeError;

            fn $method(
                &mut self,
                _driver: &ValueDriver<'_, 'b, 'l>,
                value: $ty,
            ) -> Result<Self::Value, Self::Error> {
                Ok(value)
            }

            visitor_default! { <'b, 'l> $($defaulted),* = Err(DecodeError::UnexpectedType { expected: $label }) }
        }
    };
}

leaf_visitor!(
    U8V, visit_u8, u8, "u8", u16, u32, u64, u128, u256, bool, address, signer, vector, struct,
    variant
);
leaf_visitor!(
    U16V, visit_u16, u16, "u16", u8, u32, u64, u128, u256, bool, address, signer, vector, struct,
    variant
);
leaf_visitor!(
    U32V, visit_u32, u32, "u32", u8, u16, u64, u128, u256, bool, address, signer, vector, struct,
    variant
);
leaf_visitor!(
    U64V, visit_u64, u64, "u64", u8, u16, u32, u128, u256, bool, address, signer, vector, struct,
    variant
);
leaf_visitor!(
    U128V, visit_u128, u128, "u128", u8, u16, u32, u64, u256, bool, address, signer, vector,
    struct, variant
);
leaf_visitor!(
    U256V, visit_u256, U256, "u256", u8, u16, u32, u64, u128, bool, address, signer, vector,
    struct, variant
);
leaf_visitor!(
    BoolV, visit_bool, bool, "bool", u8, u16, u32, u64, u128, u256, address, signer, vector,
    struct, variant
);
leaf_visitor!(
    AddressV,
    visit_address,
    AccountAddress,
    "address",
    u8,
    u16,
    u32,
    u64,
    u128,
    u256,
    bool,
    signer,
    vector,
    struct,
    variant
);

/// Consume the next field of `driver` with `visitor`.
fn take_field<'b, 'l, V>(
    driver: &mut StructDriver<'_, 'b, 'l>,
    mut visitor: V,
) -> Result<V::Value, DecodeError>
where
    V: Visitor<'b, 'l, Error = DecodeError>,
{
    match driver.next_field(&mut visitor)? {
        Some((_field, value)) => Ok(value),
        None => Err(DecodeError::UnexpectedEnd),
    }
}

macro_rules! scalar_field {
    ($fn_name:ident, $visitor:ident, $ty:ty) => {
        #[doc = concat!("Read the next field as `", stringify!($ty), "`.")]
        pub fn $fn_name(driver: &mut StructDriver<'_, '_, '_>) -> Result<$ty, DecodeError> {
            take_field(driver, $visitor)
        }
    };
}

scalar_field!(u8_field, U8V, u8);
scalar_field!(u16_field, U16V, u16);
scalar_field!(u32_field, U32V, u32);
scalar_field!(u64_field, U64V, u64);
scalar_field!(u128_field, U128V, u128);
scalar_field!(u256_field, U256V, U256);
scalar_field!(bool_field, BoolV, bool);
scalar_field!(address_field, AddressV, AccountAddress);

/// Read the next field as a borrowed `vector<u8>`.
pub fn bytes_field<'b>(driver: &mut StructDriver<'_, 'b, '_>) -> Result<&'b [u8], DecodeError> {
    take_field(driver, BytesV)
}

/// Read the next field as a `vector<u64>`.
pub fn u64_vec_field(driver: &mut StructDriver<'_, '_, '_>) -> Result<Vec<u64>, DecodeError> {
    take_field(driver, VecVisitor(U64V))
}

/// Read the next field as a `vector<u128>`.
pub fn u128_vec_field(driver: &mut StructDriver<'_, '_, '_>) -> Result<Vec<u128>, DecodeError> {
    take_field(driver, VecVisitor(U128V))
}

/// Read the next field as a `vector<address>`.
pub fn address_vec_field(
    driver: &mut StructDriver<'_, '_, '_>,
) -> Result<Vec<AccountAddress>, DecodeError> {
    take_field(driver, VecVisitor(AddressV))
}

/// Read the next field as a nested struct using `decoder`.
pub fn struct_field<'b, 'l, D>(
    driver: &mut StructDriver<'_, 'b, 'l>,
    decoder: D,
) -> Result<D::Output, DecodeError>
where
    D: StructDecoder<'b, 'l>,
{
    take_field(driver, StructVisitor(decoder))
}

/// Read the next field as a `vector` of nested structs using `decoder`.
pub fn struct_vec_field<'b, 'l, D>(
    driver: &mut StructDriver<'_, 'b, 'l>,
    decoder: D,
) -> Result<Vec<D::Output>, DecodeError>
where
    D: StructDecoder<'b, 'l> + Copy,
{
    take_field(driver, VecVisitor(StructVisitor(decoder)))
}

/// Decode a whole struct from its BCS bytes.
pub fn decode_struct<'b, 'l, D>(
    bytes: &'b [u8],
    layout: &'l MoveStructLayout,
    decoder: D,
) -> Result<D::Output, DecodeError>
where
    D: StructDecoder<'b, 'l>,
{
    let mut visitor = StructVisitor(decoder);
    MoveStruct::visit_deserialize(bytes, layout, &mut visitor)
}

/// Decode any value from its BCS bytes using a caller-supplied visitor.
pub fn decode_value<'b, 'l, V>(
    bytes: &'b [u8],
    layout: &'l MoveTypeLayout,
    mut visitor: V,
) -> Result<V::Value, DecodeError>
where
    V: Visitor<'b, 'l, Error = DecodeError>,
{
    MoveValue::visit_deserialize(bytes, layout, &mut visitor)
}

/// Read a struct field that must be present, returning a typed "missing" error otherwise.
///
/// Decoders use this to convert `Option`s collected in a `while let` loop into either the value or
/// a hard failure naming the field.
pub fn required<T>(value: Option<T>, field: &'static str) -> Result<T, DecodeError> {
    value.ok_or(DecodeError::MissingField(field))
}

/// Reject a variant value: none of the venues in scope are enums, and silently ignoring one would
/// hide a layout change.
pub fn reject_variant(driver: &mut VariantDriver<'_, '_, '_>) -> Result<(), DecodeError> {
    let _ = driver.variant_name();
    Err(DecodeError::UnexpectedType {
        expected: "a struct (enums are not supported by this decoder)",
    })
}
