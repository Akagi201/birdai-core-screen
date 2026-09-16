//! Venue typing and the on-chain price-discovery classifier.
//!
//! Three Sui objects, three answers:
//!
//! | object | type | venue? | why |
//! |---|---|---|---|
//! | A | `…::pool::Pool<USDC, SUI>` | **yes** | quotes `sqrt_price` from its own liquidity and moves it with flow |
//! | B | `…::native_pool::NativePool` | no | the SUI↔VSUI rate is an accounting ratio, not a quote |
//! | C | `…::storage::Storage` | no | asset values are imported from an oracle; interest is a curve |
//!
//! The test is structural and behavioural, never name-based. See [`classify`] for the three probes
//! and [`classify::Verdict::render`] for the evidence each one prints.
//!
//! # Typing is name-based on purpose
//!
//! Every decoder here matches fields **by name**, which is what lets an upgraded package that
//! appends or reorders fields keep working. A field that disappears is a hard
//! [`birdai_move::DecodeError::MissingField`] — silence would mean pricing against a stale schema.

pub mod cetus;
pub mod classify;
pub mod error;
pub mod navi;
pub mod venue;
pub mod volo;

pub use cetus::{
    CETUS_POOL_MODULE, CETUS_POOL_NAME, CetusClmm, CetusPoolDecoder, MAX_TICK_CROSSINGS,
};
pub use classify::{
    Classifier, EntryEvidence, OracleDenySet, OracleReference, OracleVia, PriceObservation,
    PriceStateChange, Probe, StaticEvidence, SwapEntry, Verdict, coin_signatures,
    find_inter_asset_swap, held_asset, price_state_change, referenced_types,
    scan_layout_for_oracles, scan_module_dependencies,
};
pub use error::VenueError;
pub use navi::{NAVI_STORAGE_MODULE, NAVI_STORAGE_NAME, NaviStorage};
pub use venue::{
    AnyVenue, PriceState, Venue, VenueKind, decode_venue, layout_has_shape, venue_kind_of,
};
pub use volo::{VOLO_POOL_MODULE, VOLO_POOL_NAME, VoloNativePool};
