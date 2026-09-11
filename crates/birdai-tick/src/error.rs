//! Errors from reading or validating a tick index.

use thiserror::Error;

/// Something is wrong with the pool's tick skip list.
///
/// Every variant here means the index cannot be trusted to price against, which is why they are
/// hard errors rather than skipped rows: a tick index that is *nearly* right silently misprices
/// every quote that depends on it.
#[derive(Debug, Error)]
pub enum TickError {
    /// A tick index fell outside the range the bias can encode.
    #[error("tick {0} cannot be encoded as a skip-list score")]
    TickOutOfRange(i32),

    /// The skip list's metadata disagrees with the number of nodes we managed to read.
    ///
    /// This is the signature of an incomplete read — a truncated page walk, or a resync that
    /// dropped children.
    #[error("skip list reports {expected} nodes but {actual} were decoded")]
    SizeMismatch {
        /// The count in the skip list metadata.
        expected: u64,
        /// The number of nodes actually decoded.
        actual: usize,
    },

    /// A node points at a neighbour that is not in the index.
    #[error("node {score} links to {neighbour}, which is not in the index")]
    DanglingLink {
        /// The node holding the link.
        score: u64,
        /// The missing neighbour.
        neighbour: u64,
    },

    /// A node's key does not encode its tick index.
    ///
    /// `score` must equal `tick_index + 443_636`; anything else means the walk mixed up two
    /// ticks or the node was misread.
    #[error("node with key {score} claims tick index {tick}")]
    ScoreMismatch {
        /// The tick index the node stores.
        tick: i32,
        /// The key the node was filed under.
        score: u64,
    },

    /// The same node was decoded twice, which means the page walk repeated itself.
    #[error("node {0} was decoded more than once")]
    DuplicateNode(u64),

    /// The level-0 chain is not ordered by tick index.
    #[error("the skip list's level-0 chain is not ordered by tick index")]
    Unordered,

    /// An initialised tick sits off the pool's spacing grid.
    ///
    /// Ticks initialise only on multiples of `tick_spacing`; anything else means the node was
    /// misread or does not belong to this pool.
    #[error("tick {tick} is not on the spacing grid of {spacing}")]
    OffGrid {
        /// The offending tick index.
        tick: i32,
        /// The pool's tick spacing.
        spacing: u32,
    },

    /// A tick node's stored square-root price is further from the tick math than rounding allows.
    ///
    /// Cetus stores the price at every initialised tick, so it can be checked against
    /// [`birdai_amm::sqrt_price_at_tick`]. That function is accurate to
    /// [`birdai_amm::TICK_PRICE_TOLERANCE`] ulp of the on-chain value, so only a larger deviation —
    /// which would mean the node was misread or the tick index does not belong to it — is an error.
    #[error(
        "tick {tick} stores sqrt price {stored}, which is {deviation} away from the tick math's \
         {computed} (tolerance is ±{tolerance})"
    )]
    PriceMismatch {
        /// The tick index.
        tick: i32,
        /// The price stored in the node.
        stored: u128,
        /// The price derived from the index.
        computed: u128,
        /// `stored - computed`, signed.
        deviation: i128,
        /// The tolerance that was exceeded.
        tolerance: u128,
    },

    /// Decoding a node's BCS bytes failed.
    #[error(transparent)]
    Decode(#[from] birdai_move::DecodeError),

    /// The tick math could not evaluate a tick.
    #[error(transparent)]
    Amm(#[from] birdai_amm::AmmError),
}
