//! `StructTag` helpers shared by every crate that has to talk about Move types.

use std::fmt::Write as _;

use move_core_types::{
    account_address::AccountAddress,
    language_storage::{StructTag, TypeTag},
};

/// The Move standard library address (`0x1`).
pub const MOVE_STDLIB: AccountAddress = AccountAddress::ONE;

/// The Sui framework address (`0x2`).
pub const SUI_FRAMEWORK: AccountAddress = AccountAddress::TWO;

/// Sui framework modules whose structs are containers of **dynamic fields** rather than inline
/// data.
///
/// A parent's BCS only carries `{ id: UID, size: u64 }` for these; the entries live in separate
/// dynamic-field objects keyed by the inner UID. Decoders must never treat `size` as contents, and
/// dumps label these fields so a human reading the output is not misled.
pub const CHILD_CONTAINER_MODULES: &[(&str, &str)] = &[
    ("table", "Table"),
    ("bag", "Bag"),
    ("object_bag", "ObjectBag"),
    ("table_vec", "TableVec"),
    ("linked_table", "LinkedTable"),
    ("dynamic_field", "Field"),
    ("dynamic_object_field", "Wrapper"),
];

/// True when `tag` is a container whose entries are separate dynamic-field objects.
#[must_use]
pub fn is_child_container(tag: &StructTag) -> bool {
    // The Cetus skip list is not a Sui container but behaves exactly like one.
    if is_cetus_skip_list(tag) {
        return true;
    }
    tag.address == SUI_FRAMEWORK &&
        CHILD_CONTAINER_MODULES
            .iter()
            .any(|(module, name)| tag.module.as_str() == *module && tag.name.as_str() == *name)
}

/// True when `tag` is Cetus's `skip_list::SkipList<T>`.
#[must_use]
pub fn is_cetus_skip_list(tag: &StructTag) -> bool {
    tag.module.as_str() == "skip_list" && tag.name.as_str() == "SkipList"
}

/// True when `tag` is exactly `address::module::name` (type arguments ignored).
#[must_use]
pub fn is_tag(tag: &StructTag, address: AccountAddress, module: &str, name: &str) -> bool {
    tag.address == address && tag.module.as_str() == module && tag.name.as_str() == name
}

/// True when `tag` is `0x2::coin::Coin<T>` and returns `T`.
#[must_use]
pub fn coin_inner(tag: &StructTag) -> Option<&TypeTag> {
    if is_tag(tag, SUI_FRAMEWORK, "coin", "Coin") { tag.type_params.first() } else { None }
}

/// True when `tag` is `0x2::balance::Balance<T>` and returns `T`.
#[must_use]
pub fn balance_inner(tag: &StructTag) -> Option<&TypeTag> {
    if is_tag(tag, SUI_FRAMEWORK, "balance", "Balance") { tag.type_params.first() } else { None }
}

/// Render an address the way Sui's canonical `Display` does: full 32-byte hex, no truncation.
#[must_use]
pub fn full_address(address: &AccountAddress) -> String {
    address.to_canonical_string(true)
}

/// Render a `StructTag` with abbreviated addresses, for dumps and log lines.
///
/// `0x0000…0002::coin::Coin<0x0000…0002::sui::SUI>` becomes `0x2::coin::Coin<0x2::sui::SUI>`.
#[must_use]
pub fn short_tag(tag: &StructTag) -> String {
    let mut out = String::new();
    out.push_str(&short_address(&tag.address));
    out.push_str("::");
    out.push_str(tag.module.as_str());
    out.push_str("::");
    out.push_str(tag.name.as_str());
    if !tag.type_params.is_empty() {
        out.push('<');
        for (index, param) in tag.type_params.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&short_type(param));
        }
        out.push('>');
    }
    out
}

/// Render a `TypeTag` with abbreviated addresses.
#[must_use]
pub fn short_type(tag: &TypeTag) -> String {
    match tag {
        TypeTag::Vector(inner) => format!("vector<{}>", short_type(inner)),
        TypeTag::Struct(inner) => short_tag(inner),
        other => other.to_string(),
    }
}

/// Abbreviate a 32-byte address to its significant leading digits.
#[must_use]
pub fn short_address(address: &AccountAddress) -> String {
    let hex = format!("{address}");
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0x0".to_owned();
    }
    let mut out = String::with_capacity(trimmed.len() + 2);
    let _ = write!(out, "0x{trimmed}");
    out
}
