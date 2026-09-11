//! Errors produced while decoding Move values into typed Rust structures.

use thiserror::Error;

/// Everything that can go wrong when a layout-driven visitor turns BCS bytes into a typed value.
///
/// The variants are deliberately coarse: the decoder is layout-driven, so almost every failure is
/// "the on-chain layout is not what this decoder expects", and the payloads exist to make that
/// diagnosable rather than to be matched on.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// A struct field the decoder requires was not present in the layout.
    ///
    /// This is the signal that a package upgrade renamed or removed a field, which the state
    /// manager treats as a hard error rather than silently serving stale data.
    #[error("required field `{0}` is missing from the struct layout")]
    MissingField(&'static str),

    /// The layout did not have the shape this decoder requires (for example, a scalar where a
    /// struct was expected).
    #[error("expected {expected}, found a different Move type")]
    UnexpectedType {
        /// Human-readable description of what the decoder wanted.
        expected: &'static str,
    },

    /// The BCS byte stream ended before a value could be read.
    #[error("unexpected end of BCS input")]
    UnexpectedEnd,

    /// Two's-complement bits could not be reinterpreted because the width was wrong.
    #[error("value out of range for {target}")]
    OutOfRange {
        /// The Rust type that rejected the value.
        target: &'static str,
    },
}

impl From<move_core_types::annotated_visitor::Error> for DecodeError {
    fn from(err: move_core_types::annotated_visitor::Error) -> Self {
        // The annotated visitor's errors are all "the bytes did not match the layout", which from
        // this crate's point of view is an unexpected type. Keep the message reachable for logs.
        tracing::debug!(error = %err, "annotation error while decoding");
        Self::UnexpectedType { expected: "a well-formed value matching the layout" }
    }
}
