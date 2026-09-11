//! Errors from applying checkpoint data to in-memory venue state.

use thiserror::Error;

/// Something went wrong while keeping state current.
#[derive(Debug, Error)]
pub enum StateError {
    /// A package version observed in the change set could not be read.
    #[error("could not read package {0}")]
    Package(String),

    /// An object in the change set could not be found in the checkpoint's object set.
    #[error("object {id} at version {version} is missing from the checkpoint's object set")]
    MissingObject {
        /// The object id.
        id: String,
        /// The version the effects pointed at.
        version: u64,
    },

    /// A slot was asked to accept an older version of an object than it already holds.
    ///
    /// This is the ordering hazard: applying changes in transaction order must be monotone in
    /// version. Seeing an older version means changes were applied twice or out of order, which
    /// would silently serve a stale price.
    #[error(
        "object {id} would move backwards from version {current} to {incoming}; \
         the checkpoint was applied out of order"
    )]
    OutOfOrder {
        /// The object id.
        id: String,
        /// The version already held.
        current: u64,
        /// The version being applied.
        incoming: u64,
    },

    /// A layout could not be resolved while typing an object.
    #[error(transparent)]
    Resolve(#[from] birdai_resolve::ResolveError),

    /// An object could not be typed as a venue.
    #[error(transparent)]
    Venue(#[from] birdai_venue::VenueError),

    /// The tick index of a pool could not be built.
    #[error(transparent)]
    Tick(#[from] birdai_tick::TickError),
}
