//! Cetus's concentrated-liquidity tick index: a skip list of initialised ticks.
//!
//! # What the object actually holds
//!
//! `tick_manager.ticks` is a `skip_list::SkipList<Tick>` **inlined** in the pool. Its BCS carries
//! only the list's own metadata — `head`, `tail`, `level`, `max_level`, `list_p`, `size`, `random`
//! — plus the `UID` that owns the nodes:
//!
//! ```text
//! SkipList<Tick> { id: UID, head: vector<OptionU64>, tail: OptionU64,
//!                  level: u64, max_level: u64, list_p: u64, size: u64, random: Random { seed } }
//! Node<Tick>     { score: u64, nexts: vector<OptionU64>, prev: OptionU64, value: Tick }
//! Tick           { index: I32, sqrt_price: u128, liquidity_net: I128, liquidity_gross: u128, .. }
//! ```
//!
//! The nodes are **dynamic fields attached to the skip list's inner UID**, not to the pool object,
//! so `object(address: POOL) { dynamicFields }` returns nothing. They have to be reached through
//! the inner UID (see [`crate::Ticks::node_uid`]).
//!
//! # `score`
//!
//! Nodes are keyed by a `u64` `score`, not by the tick index: `score = tick_index + 443_636`,
//! Cetus's `MAX_TICK` bias that turns a signed tick into an orderable key. Verified on mainnet —
//! node `509_466` carries `index.bits = 65_830`, and `65_830 + 443_636 = 509_466`.

pub mod error;
pub mod index;

#[cfg(test)]
mod accessor_tests;
#[cfg(test)]
mod decode_tests;

pub use error::TickError;
pub use index::{
    CETUS_TICK_BIAS, FieldNodeDecoder, IdDecoder, Locate, NodeUid, SizeSkew, SkipListDecoder,
    SkipListHead, Tick, TickManagerDecoder, TickNode, TickNodeDecoder, Ticks, UidDecoder,
    score_from_tick, tick_from_score,
};
