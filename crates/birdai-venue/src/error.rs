//! Errors from typing, quoting against, or classifying an object.

use thiserror::Error;

/// Something went wrong while treating an object as a venue.
#[derive(Debug, Error)]
pub enum VenueError {
    /// The object's type is not one this crate knows how to type.
    #[error("`{0}` is not a venue type this crate understands")]
    UnknownVenue(String),

    /// The object has no type at all, so it cannot be a venue.
    #[error("object {0} has no type tag")]
    NoTypeTag(String),

    /// The layout handed in does not describe a struct.
    #[error("the layout for `{tag}` is not a struct")]
    NotAStruct {
        /// The type whose layout was not a struct.
        tag: String,
    },

    /// The pool's state cannot be priced as given.
    #[error("pool is not priceable: {0}")]
    NotPriceable(String),

    /// Reading the defining package failed.
    #[error("could not read package {package}: {message}")]
    Package {
        /// The package address.
        package: String,
        /// Why it failed.
        message: String,
    },

    /// The defining package has no such module.
    #[error("package {package} has no module `{module}`")]
    Module {
        /// The package address.
        package: String,
        /// The module name.
        module: String,
    },

    /// Reading a function definition out of the package bytecode failed.
    #[error("could not read function `{function}` of {module}: {message}")]
    Function {
        /// The module the function should be in.
        module: String,
        /// The function name.
        function: String,
        /// Why it failed.
        message: String,
    },

    /// AM1M arithmetic failed while quoting.
    #[error(transparent)]
    Amm(#[from] birdai_amm::AmmError),

    /// Decoding the object's BCS bytes failed.
    #[error(transparent)]
    Decode(#[from] birdai_move::DecodeError),

    /// Resolving a layout failed.
    #[error(transparent)]
    Resolve(#[from] birdai_resolve::ResolveError),
}
