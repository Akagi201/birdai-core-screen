//! Errors from object and layout resolution.

use thiserror::Error;

/// Something went wrong while turning an object id into a typed, layout-resolved value.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// The package resolver could not produce a layout for a type.
    #[error("could not resolve a layout for `{tag}`: {source}")]
    Layout {
        /// The type that could not be resolved.
        tag: String,
        /// The underlying resolver failure.
        source: sui_package_resolver::error::Error,
    },

    /// A package does not contain the module the caller asked for.
    #[error("package {package} has no module `{module}`")]
    ModuleNotFound {
        /// The package address.
        package: String,
        /// The module name.
        module: String,
    },

    /// The node does not know the requested object, at the requested version.
    #[error("object {id} not found{}", version.map_or(String::new(), |v| format!(" at version {v}")))]
    ObjectNotFound {
        /// The object id.
        id: String,
        /// The requested version, when one was given.
        version: Option<u64>,
    },

    /// An RPC call failed for a reason other than "not found".
    #[error("rpc call `{call}` failed: {message}")]
    Rpc {
        /// Which call failed.
        call: &'static str,
        /// The transport-level message.
        message: String,
    },

    /// The object exists but is a package, not a Move struct.
    #[error("object {0} is not a Move object")]
    NotAMoveObject(String),

    /// The object exists but has no type, so it cannot be laid out.
    #[error("object {0} has no type tag")]
    NoTypeTag(String),

    /// A gRPC response did not carry a field we require.
    #[error("gRPC response for `{call}` was missing `{field}`")]
    MissingResponseField {
        /// The call that returned the incomplete response.
        call: &'static str,
        /// The absent field.
        field: &'static str,
    },

    /// A page token or identifier from the node could not be parsed.
    #[error("node returned an unparsable {what}: {value}")]
    Unparsable {
        /// What kind of value it was.
        what: &'static str,
        /// The offending text.
        value: String,
    },

    /// A fixture set could not be read, or does not hold what the run needs.
    #[error(transparent)]
    Fixture(Box<crate::fixture::FixtureError>),
}
