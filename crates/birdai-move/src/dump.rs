//! A field-by-field annotated dump of a Move value.
//!
//! This is the human-facing half of decoding: it walks the layout and the bytes together and
//! records, for every value it sees, the **exact BCS byte range** it came from. That range is not
//! decoration — [`tests::byte_ranges_slice_back_to_the_same_values`] cuts each leaf out of the
//! object's bytes by its recorded range and re-decodes it, which is what makes the dump
//! trustworthy enough to quote in the README.

use std::ops::Range;

use move_core_types::{
    account_address::AccountAddress,
    annotated_value::MoveTypeLayout,
    annotated_visitor::{StructDriver, ValueDriver, VariantDriver, VecDriver, Visitor},
    u256::U256,
};

use crate::{
    error::DecodeError,
    tag::{is_child_container, short_tag},
};

/// How much of a value to render.
#[derive(Debug, Clone, Copy)]
pub struct DumpOptions {
    /// Stop recursing below this depth; deeper values are summarised instead.
    pub max_depth: usize,
    /// Render at most this many elements of any vector.
    pub max_items: usize,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self { max_depth: 24, max_items: 32 }
    }
}

/// A byte range within the object's BCS contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Inclusive start offset.
    pub start: usize,
    /// Exclusive end offset.
    pub end: usize,
}

impl Span {
    /// Construct a span.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True when the span covers no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// The span as a standard range.
    #[must_use]
    pub const fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:>5}..{:>5}]", self.start, self.end)
    }
}

/// A decoded value, rendered for humans.
#[derive(Debug, Clone)]
pub enum DumpValue {
    /// A scalar (`bool`, integer, `address` or `signer`).
    Scalar {
        /// Compact rendering of the value's Move type.
        layout: String,
        /// The value itself.
        text: String,
    },
    /// A struct, with its fields in serialisation order.
    Struct {
        /// Abbreviated `StructTag`.
        tag: String,
        /// True when this is a dynamic-field container, so a reader is not misled by `size`.
        child_container: bool,
        /// Fields, in the order they were serialised.
        fields: Vec<DumpField>,
    },
    /// A vector.
    Vector {
        /// Compact rendering of the element type.
        element: String,
        /// Declared length.
        len: u64,
        /// Rendered elements (possibly fewer than `len` when truncated).
        items: Vec<DumpItem>,
        /// How many elements were omitted by the item cap.
        omitted: u64,
    },
    /// An enum variant. None of the objects in scope use enums, but the framework supports them.
    Variant {
        /// Variant name.
        name: String,
        /// Variant tag as serialised.
        tag: u16,
        /// Variant fields.
        fields: Vec<DumpField>,
    },
    /// A value that was not descended into because the depth budget ran out.
    DepthLimited {
        /// Compact rendering of the value's Move type.
        layout: String,
    },
}

/// A named field inside a struct or variant.
#[derive(Debug, Clone)]
pub struct DumpField {
    /// Field name.
    pub name: String,
    /// Byte range of the field's value.
    pub span: Span,
    /// The field's value.
    pub value: DumpValue,
}

/// An indexed element of a vector.
#[derive(Debug, Clone)]
pub struct DumpItem {
    /// Element index.
    pub index: usize,
    /// Byte range of the element.
    pub span: Span,
    /// The element's value.
    pub value: DumpValue,
}

impl DumpValue {
    /// Render this value and its children as indented lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.render_into(0, &mut out);
        out
    }

    /// Find a child field by name, one level deep.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Struct { fields, .. } | Self::Variant { fields, .. } => {
                fields.iter().find(|field| field.name == name).map(|field| &field.value)
            }
            _ => None,
        }
    }

    /// Follow a dotted path of field names, e.g. `"tick_manager.ticks.size"`.
    #[must_use]
    pub fn path(&self, path: &str) -> Option<&Self> {
        let mut current = self;
        for segment in path.split('.') {
            current = current.child(segment)?;
        }
        Some(current)
    }

    /// The rendered scalar text of this value, if it is a scalar.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    fn render_into(&self, indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        match self {
            Self::Scalar { layout, text } => out.push(format!("{pad}{text}  <{layout}>")),
            Self::Struct { tag, child_container, fields } => {
                if *child_container {
                    out.push(format!(
                        "{pad}{tag}  {{dynamic-field container: entries are separate objects}}"
                    ));
                } else {
                    out.push(format!("{pad}{tag}"));
                }
                for field in fields {
                    out.push(format!("{pad}  {:<28} {} =", field.name, field.span));
                    field.value.render_into(indent + 2, out);
                }
            }
            Self::Variant { name, tag, fields } => {
                out.push(format!("{pad}@{name} (variant tag {tag})"));
                for field in fields {
                    out.push(format!("{pad}  {:<28} {} =", field.name, field.span));
                    field.value.render_into(indent + 2, out);
                }
            }
            Self::Vector { element, len, items, omitted } => {
                out.push(format!("{pad}vector<{element}> (len {len})"));
                for item in items {
                    out.push(format!("{pad}  [{:>5}] {}", item.index, item.span));
                    item.value.render_into(indent + 2, out);
                }
                if *omitted > 0 {
                    out.push(format!("{pad}  … {omitted} more omitted by the item cap"));
                }
            }
            Self::DepthLimited { layout } => {
                out.push(format!("{pad}… <{layout}> (depth limit reached)"));
            }
        }
    }
}

/// Compact rendering of a `MoveTypeLayout`.
#[must_use]
pub fn short_type_of(layout: &MoveTypeLayout) -> String {
    match layout {
        MoveTypeLayout::Bool => "bool".to_owned(),
        MoveTypeLayout::U8 => "u8".to_owned(),
        MoveTypeLayout::U16 => "u16".to_owned(),
        MoveTypeLayout::U32 => "u32".to_owned(),
        MoveTypeLayout::U64 => "u64".to_owned(),
        MoveTypeLayout::U128 => "u128".to_owned(),
        MoveTypeLayout::U256 => "u256".to_owned(),
        MoveTypeLayout::Address => "address".to_owned(),
        MoveTypeLayout::Signer => "signer".to_owned(),
        MoveTypeLayout::Vector(inner) => format!("vector<{}>", short_type_of(inner)),
        MoveTypeLayout::Struct(inner) => short_tag(&inner.type_),
        MoveTypeLayout::Enum(inner) => short_tag(&inner.type_),
    }
}

/// A [`Visitor`] that builds a [`DumpValue`] tree annotated with byte ranges.
///
/// The visitor is cloned per nesting level so that each level can record the span of the value it
/// just produced without clobbering its parent's. Containers read the child's span immediately
/// after the child visitor returns, which is why the field has to live on the visitor rather than
/// on the produced value.
#[derive(Debug, Clone)]
pub struct Dump {
    options: DumpOptions,
    depth: usize,
    last_span: Span,
}

impl Default for Dump {
    fn default() -> Self {
        Self::new()
    }
}

impl Dump {
    /// A dump visitor with the default options.
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(DumpOptions::default())
    }

    /// A dump visitor with explicit options.
    #[must_use]
    pub const fn with_options(options: DumpOptions) -> Self {
        Self { options, depth: 0, last_span: Span::new(0, 0) }
    }

    /// The span of the most recently produced value.
    #[must_use]
    pub const fn last_span(&self) -> Span {
        self.last_span
    }

    const fn exhausted(&self) -> bool {
        self.depth >= self.options.max_depth
    }

    const fn deeper(&self) -> Self {
        Self { options: self.options, depth: self.depth + 1, last_span: self.last_span }
    }

    fn mark(&mut self, driver: &ValueDriver<'_, '_, '_>) {
        self.last_span = Span::new(driver.start(), driver.position());
    }

    fn scalar<T: std::fmt::Display>(
        &mut self,
        driver: &ValueDriver<'_, '_, '_>,
        value: &T,
    ) -> DumpValue {
        self.mark(driver);
        DumpValue::Scalar {
            layout: driver.layout().map_or_else(|_| "?".to_owned(), short_type_of),
            text: value.to_string(),
        }
    }
}

impl<'b, 'l> Visitor<'b, 'l> for Dump {
    type Value = DumpValue;
    type Error = DecodeError;

    fn visit_u8(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: u8,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_u16(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: u16,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_u32(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: u32,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_u64(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: u64,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_u128(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: u128,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_u256(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: U256,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_bool(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: bool,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_address(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: AccountAddress,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_signer(
        &mut self,
        driver: &ValueDriver<'_, 'b, 'l>,
        value: AccountAddress,
    ) -> Result<Self::Value, Self::Error> {
        Ok(self.scalar(driver, &value))
    }

    fn visit_vector(
        &mut self,
        driver: &mut VecDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        let element = short_type_of(driver.element_layout());
        let len = driver.len();
        let start = driver.start();

        if self.exhausted() {
            while driver.skip_element()? {}
            self.last_span = Span::new(start, driver.position());
            return Ok(DumpValue::DepthLimited { layout: format!("vector<{element}> (len {len})") });
        }

        let cap = u64::try_from(self.options.max_items).unwrap_or(u64::MAX);
        let mut inner = self.deeper();
        let mut items = Vec::new();
        let mut rendered = 0_u64;
        while let Some(value) = driver.next_element(&mut inner)? {
            if rendered < cap {
                items.push(DumpItem { index: items.len(), span: inner.last_span, value });
                rendered += 1;
                if rendered == cap {
                    // Stop producing values; the driver still walks the rest of the vector, it
                    // just does not build anything for it.
                    while driver.skip_element()? {}
                    break;
                }
            }
        }

        self.last_span = Span::new(start, driver.position());
        Ok(DumpValue::Vector { element, len, omitted: len.saturating_sub(rendered), items })
    }

    fn visit_struct(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        let tag = driver.struct_layout().type_.clone();
        let start = driver.start();

        if self.exhausted() {
            while driver.skip_field()?.is_some() {}
            self.last_span = Span::new(start, driver.position());
            return Ok(DumpValue::DepthLimited { layout: short_tag(&tag) });
        }

        let mut inner = self.deeper();
        let mut fields = Vec::new();
        while let Some((field_layout, value)) = driver.next_field(&mut inner)? {
            fields.push(DumpField {
                name: field_layout.name.as_str().to_owned(),
                span: inner.last_span,
                value,
            });
        }

        self.last_span = Span::new(start, driver.position());
        Ok(DumpValue::Struct {
            tag: short_tag(&tag),
            child_container: is_child_container(&tag),
            fields,
        })
    }

    fn visit_variant(
        &mut self,
        driver: &mut VariantDriver<'_, 'b, 'l>,
    ) -> Result<Self::Value, Self::Error> {
        let name = driver.variant_name().as_str().to_owned();
        let tag = driver.tag();
        let start = driver.start();

        if self.exhausted() {
            while driver.skip_field()?.is_some() {}
            self.last_span = Span::new(start, driver.position());
            return Ok(DumpValue::DepthLimited { layout: name });
        }

        let mut inner = self.deeper();
        let mut fields = Vec::new();
        while let Some((field_layout, value)) = driver.next_field(&mut inner)? {
            fields.push(DumpField {
                name: field_layout.name.as_str().to_owned(),
                span: inner.last_span,
                value,
            });
        }

        self.last_span = Span::new(start, driver.position());
        Ok(DumpValue::Variant { name, tag, fields })
    }
}

#[cfg(test)]
mod tests {
    use move_core_types::{
        account_address::AccountAddress,
        annotated_value::{MoveFieldLayout, MoveStructLayout, MoveTypeLayout},
        identifier::Identifier,
    };

    use super::{Dump, DumpField, DumpOptions, DumpValue, Span};
    use crate::visitor::decode_value;

    /// `Outer { a: u64, items: vector<u64>, inner: Inner { x: u32, ys: vector<address> } }`, as
    /// BCS: 8 bytes, a length-prefixed vector of two, a `u32`, and a length-prefixed vector of one.
    fn bcs_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7_u64.to_le_bytes()); // a          [ 0.. 8)
        bytes.push(2); // items length                              [ 8.. 9)
        bytes.extend_from_slice(&1_u64.to_le_bytes()); //           [ 9..17)
        bytes.extend_from_slice(&2_u64.to_le_bytes()); //           [17..25)
        bytes.extend_from_slice(&9_u32.to_le_bytes()); // inner.x   [25..29)
        bytes.push(1); // ys length                                 [29..30)
        bytes.extend_from_slice(AccountAddress::new([0xab; 32]).as_ref()); // [30..62)
        bytes
    }

    fn ident(text: &str) -> Identifier {
        Identifier::new(text).unwrap_or_else(|_| unreachable!())
    }

    fn struct_layout(
        module: &str,
        name: &str,
        fields: Vec<(&str, MoveTypeLayout)>,
    ) -> MoveTypeLayout {
        MoveTypeLayout::Struct(Box::new(MoveStructLayout {
            type_: move_core_types::language_storage::StructTag {
                address: AccountAddress::TWO,
                module: ident(module),
                name: ident(name),
                type_params: vec![],
            },
            fields: fields
                .into_iter()
                .map(|(name, layout)| MoveFieldLayout { name: ident(name), layout })
                .collect(),
        }))
    }

    fn layout() -> MoveTypeLayout {
        struct_layout(
            "outer",
            "Outer",
            vec![
                ("a", MoveTypeLayout::U64),
                ("items", MoveTypeLayout::Vector(Box::new(MoveTypeLayout::U64))),
                (
                    "inner",
                    struct_layout(
                        "inner",
                        "Inner",
                        vec![
                            ("x", MoveTypeLayout::U32),
                            ("ys", MoveTypeLayout::Vector(Box::new(MoveTypeLayout::Address))),
                        ],
                    ),
                ),
            ],
        )
    }

    fn field<'a>(value: &'a DumpValue, name: &str) -> &'a DumpField {
        let DumpValue::Struct { fields, .. } = value else {
            unreachable!("the root and `inner` are structs")
        };
        fields.iter().find(|field| field.name == name).unwrap_or_else(|| unreachable!())
    }

    /// Re-decode the recorded slice with the leaf's own layout and render it.
    fn reread(bytes: &[u8], layout: &MoveTypeLayout, span: Span) -> String {
        decode_value(&bytes[span.range()], layout, Dump::new())
            .unwrap_or_else(|error| unreachable!("the slice must decode: {error}"))
            .lines()
            .join(" ")
    }

    #[test]
    fn byte_ranges_slice_back_to_the_same_values() {
        let bytes = bcs_bytes();
        let layout = layout();
        let dump = decode_value(&bytes, &layout, Dump::new())
            .unwrap_or_else(|error| unreachable!("{error}"));

        assert_eq!(field(&dump, "a").span, Span::new(0, 8));
        assert_eq!(
            field(&dump, "items").span,
            Span::new(8, 25),
            "a vector's span covers its prefix"
        );
        assert_eq!(field(&dump, "inner").span, Span::new(25, 62));

        let inner = field(&dump, "inner").value.clone();
        assert_eq!(field(&inner, "x").span, Span::new(25, 29));
        assert_eq!(field(&inner, "ys").span, Span::new(29, 62));

        // Every leaf, re-decoded from its own recorded slice, must produce the same render — which
        // is only true if the range is exactly the leaf's bytes.
        let DumpValue::Vector { items, .. } = &field(&dump, "items").value else {
            unreachable!("`items` is a vector")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(reread(&bytes, &MoveTypeLayout::U64, items[0].span), "1  <u64>");
        assert_eq!(reread(&bytes, &MoveTypeLayout::U64, items[1].span), "2  <u64>");
        assert_eq!(reread(&bytes, &MoveTypeLayout::U32, field(&inner, "x").span), "9  <u32>");
        let DumpValue::Vector { items, .. } = &field(&inner, "ys").value else {
            unreachable!("`ys` is a vector")
        };
        assert_eq!(items[0].span.len(), 32, "an address is 32 bytes");
        assert!(reread(&bytes, &MoveTypeLayout::Address, items[0].span).ends_with("<address>"));
    }

    #[test]
    fn a_depth_limited_value_still_reports_its_own_span() {
        // `inner.ys` sits exactly at the depth limit. Its span must still be its own, not the span
        // of the sibling that was rendered before it.
        let bytes = bcs_bytes();
        let options = DumpOptions { max_depth: 2, max_items: 32 };
        let dump = decode_value(&bytes, &layout(), Dump::with_options(options))
            .unwrap_or_else(|error| unreachable!("{error}"));

        let inner = field(&dump, "inner").value.clone();
        let ys = field(&inner, "ys");
        assert_eq!(ys.span, Span::new(29, 62));
        assert!(matches!(ys.value, DumpValue::DepthLimited { .. }));
    }
}
