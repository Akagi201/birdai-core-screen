//! The mainnet inputs this repository is about.
//!
//! Every value here was read from Sui mainnet and is asserted by tests that run against the
//! committed fixture set (`birdai-resolve`'s `committed` module and `birdai-state`'s checkpoint
//! replay), so a drift in the fixtures or in the decoders fails the build rather than quietly
//! changing the answer.

use sui_types::base_types::ObjectID;

/// Public mainnet gRPC v2 endpoint.
pub(crate) const MAINNET_RPC: &str = "https://fullnode.mainnet.sui.io:443";

/// Public mainnet **archival** gRPC endpoint.
///
/// `fullnode.mainnet.sui.io` keeps only a bounded window of checkpoints: asking for one older than
/// its retention fails, and under load it returns transient `unavailable` errors.
/// `archive.mainnet.sui.io` keeps the full history of checkpoint and object data, but does **not**
/// implement `StateService` — `ListDynamicFields` answers `Unimplemented` there. A run needing both
/// an old checkpoint and the current dynamic-field index has to talk to both. See
/// `docs.sui.io/develop/accessing-data/grpc`.
pub(crate) const MAINNET_ARCHIVE: &str = "https://archive.mainnet.sui.io:443";

/// Object A: Cetus CLMM pool `Pool<USDC, SUI>`.
pub(crate) const POOL_A: &str =
    "0x51e883ba7c0b566a26cbc8a94cd33eb0abd418a77cc1e60ad22fd9b1f29cd2ab";

/// Object A's full type, with both type arguments.
pub(crate) const POOL_A_TYPE: &str = concat!(
    "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb::pool::Pool<",
    "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC,",
    "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>"
);

/// The package that the swap transaction T actually entered through.
///
/// Recorded because it is *not* reachable from the pool's own layout: Cetus's `pool` module has no
/// swap, and the entry that moved pool A is `pool_script_v2::swap_b2a` in a sibling package. A
/// fixture that only captured what the layouts name would lose the evidence the classifier uses.
pub(crate) const POOL_SCRIPT_PACKAGE: &str =
    "0xae9c208cf58fd5ba36737c9ee5dcfa7f152d0fb5a5a99eebb7c881ebc2fe59e0";

/// Object B: Volo liquid-staking `NativePool`.
pub(crate) const POOL_B: &str =
    "0x7fa2faa111b8c65bea48a23049bfd81ca8f971a262d981dcd9a17c3825cb5baf";

/// Object C: Navi lending `Storage`.
pub(crate) const POOL_C: &str =
    "0xbb4e2f4b6205c2e2a2db47aeb4f830796ec7c005f88537ee775986639bc442fe";

/// Transaction T: a single swap on object A.
pub(crate) const TX_T: &str = "F53RBSPn84e28FDWnunb7dykGTp7sNpzEnNUxG5h5fe7";

/// The checkpoint T was executed in.
pub(crate) const TX_T_CHECKPOINT: u64 = 320_577_815;

/// The pool version T consumed, i.e. the state immediately before it.
pub(crate) const POOL_A_PRE_VERSION: u64 = 995_150_484;

/// The pool version T produced.
pub(crate) const POOL_A_POST_VERSION: u64 = 995_150_494;

/// SUI in, in MIST.
pub(crate) const TX_T_AMOUNT_IN: u128 = 100_000_000_000;

/// USDC out, in base units — what the chain actually produced.
pub(crate) const TX_T_AMOUNT_OUT: u128 = 81_168_759;

/// The fee T paid, in MIST.
pub(crate) const TX_T_FEE: u128 = 50_000_000;

/// Pool A's fee rate: 500 millionths, i.e. 5 bps.
pub(crate) const POOL_A_FEE_RATE: u64 = 500;

/// Pool A's tick spacing.
pub(crate) const POOL_A_TICK_SPACING: u32 = 10;

/// Parse one of the object ids embedded above.
///
/// Fallible rather than panicking: the ids are constants, so a parse failure is a typo in this
/// file, and the binary should say which string is wrong instead of aborting.
pub(crate) fn object_id(text: &str) -> eyre::Result<ObjectID> {
    text.parse().map_err(|error| eyre::eyre!("`{text}` is not a valid object id: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{POOL_A, POOL_A_TYPE, POOL_B, POOL_C, object_id};

    #[test]
    fn embedded_ids_parse() -> eyre::Result<()> {
        assert_eq!(object_id(POOL_A)?.to_canonical_string(true), POOL_A);
        assert_eq!(object_id(POOL_B)?.to_canonical_string(true), POOL_B);
        assert_eq!(object_id(POOL_C)?.to_canonical_string(true), POOL_C);
        Ok(())
    }

    #[test]
    fn the_pool_type_mentions_the_pool_address() {
        assert!(POOL_A_TYPE.contains("::pool::Pool<"));
        assert!(POOL_A_TYPE.contains("::usdc::USDC"));
        assert!(POOL_A_TYPE.contains("::sui::SUI>"));
    }
}
