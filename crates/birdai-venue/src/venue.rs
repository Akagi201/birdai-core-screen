//! The venue trait, the price-state shape pricing code consumes, and the decode dispatch.

use birdai_move::tag::short_tag;
use move_core_types::{
    annotated_value::{MoveStructLayout, MoveTypeLayout},
    language_storage::StructTag,
};

use crate::{
    cetus::{CETUS_POOL_MODULE, CETUS_POOL_NAME, CetusClmm},
    error::VenueError,
    navi::{NAVI_STORAGE_MODULE, NAVI_STORAGE_NAME, NaviStorage},
    volo::{VOLO_POOL_MODULE, VOLO_POOL_NAME, VoloNativePool},
};

/// The venue types this crate can type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VenueKind {
    /// Cetus concentrated-liquidity pool: `…::pool::Pool<A, B>`.
    CetusClmm,
    /// Volo liquid-staking pool: `…::native_pool::NativePool`.
    VoloNativePool,
    /// Navi lending storage: `…::storage::Storage`.
    NaviStorage,
}

impl VenueKind {
    /// A short, stable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CetusClmm => "cetus-clmm",
            Self::VoloNativePool => "volo-native-pool",
            Self::NaviStorage => "navi-storage",
        }
    }

    /// True when this kind discovers prices on chain rather than importing them.
    ///
    /// Only the Cetus pool does — see [`crate::classify`] for the reasoning and the evidence.
    #[must_use]
    pub const fn has_on_chain_price_discovery(self) -> bool {
        matches!(self, Self::CetusClmm)
    }
}

/// The part of a venue's state that determines its marginal price.
///
/// Pricing code works with this rather than with a whole venue, which keeps the state manager's
/// hot path small and makes the classification probe — "does a numeric field of the object's own
/// fields move with flow?" — expressible in one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceState {
    /// Square-root price in Q64.64, `√(b_v / a_v) · 2^64`.
    pub sqrt_price: u128,
    /// Active liquidity `L`.
    pub liquidity: u128,
    /// The tick the price currently sits in.
    pub tick: i32,
}

impl PriceState {
    /// `√P` as a real number. Display only: pricing is done in integers.
    #[must_use]
    pub fn sqrt_price_real(&self) -> f64 {
        self.sqrt_price as f64 / (birdai_amm::Q64 as f64)
    }

    /// The price `P = b_v / a_v` as a real number, in **raw** units of B per raw unit of A.
    #[must_use]
    pub fn price_raw(&self) -> f64 {
        let sqrt = self.sqrt_price_real();
        sqrt * sqrt
    }
}

/// A Sui object that this crate can type.
pub trait Venue: Sized + Send + Sync + 'static {
    /// Which kind of venue this is.
    const KIND: VenueKind;

    /// Decode the object's BCS bytes using its resolved layout.
    fn decode(bytes: &[u8], layout: &MoveTypeLayout) -> Result<Self, VenueError>;

    /// The price-determining part of the state, when the venue has one.
    ///
    /// A vault or a lending ledger returns `None`: it holds balances but does not quote a price
    /// out of its own fields.
    fn price_state(&self) -> Option<PriceState>;
}

/// True when `tag` looks like a Cetus pool, a Volo pool or a Navi storage object.
///
/// Matching is on `module::name` only, not on the package address, so a pool deployed under a new
/// package is still recognised — which is what makes new pools appear automatically in the state
/// manager.
#[must_use]
pub fn venue_kind_of(tag: &StructTag) -> Option<VenueKind> {
    match (tag.module.as_str(), tag.name.as_str()) {
        (m, n) if m == CETUS_POOL_MODULE && n == CETUS_POOL_NAME => Some(VenueKind::CetusClmm),
        (m, n) if m == VOLO_POOL_MODULE && n == VOLO_POOL_NAME => Some(VenueKind::VoloNativePool),
        (m, n) if m == NAVI_STORAGE_MODULE && n == NAVI_STORAGE_NAME => {
            Some(VenueKind::NaviStorage)
        }
        _ => None,
    }
}

/// A venue, decoded but not yet narrowed to one concrete type.
#[derive(Debug, Clone)]
pub enum AnyVenue {
    /// A Cetus concentrated-liquidity pool.
    Cetus(Box<CetusClmm>),
    /// A Volo liquid-staking pool.
    Volo(Box<VoloNativePool>),
    /// A Navi lending storage object.
    Navi(Box<NaviStorage>),
}

impl AnyVenue {
    /// Which kind this is.
    #[must_use]
    pub const fn kind(&self) -> VenueKind {
        match self {
            Self::Cetus(_) => VenueKind::CetusClmm,
            Self::Volo(_) => VenueKind::VoloNativePool,
            Self::Navi(_) => VenueKind::NaviStorage,
        }
    }

    /// The price state, if the venue has one.
    #[must_use]
    pub fn price_state(&self) -> Option<PriceState> {
        match self {
            Self::Cetus(pool) => pool.price_state(),
            Self::Volo(pool) => pool.price_state(),
            Self::Navi(storage) => storage.price_state(),
        }
    }

    /// The Cetus pool, if this is one.
    #[must_use]
    pub fn as_cetus(&self) -> Option<&CetusClmm> {
        match self {
            Self::Cetus(pool) => Some(pool),
            _ => None,
        }
    }
}

/// Decode `bytes` according to `tag`, if `tag` is a venue type **and the layout has that shape**.
///
/// The name check is a hint, not a test. More than one package on mainnet defines a struct called
/// `pool::Pool`, and they do not share a layout — typing one with the other's decoder produces a
/// `MissingField` error on every field. Requiring the resolved layout to carry the fields the venue
/// must have turns that into a clean "this is a different `Pool`", which the state manager counts
/// separately from a genuine decode failure.
pub fn decode_venue(
    bytes: &[u8],
    tag: &StructTag,
    layout: &MoveTypeLayout,
) -> Result<AnyVenue, VenueError> {
    let kind = venue_kind_of(tag).ok_or_else(|| VenueError::UnknownVenue(short_tag(tag)))?;
    let MoveTypeLayout::Struct(struct_layout) = layout else {
        return Err(VenueError::NotAStruct { tag: short_tag(tag) });
    };
    if !layout_has_shape(kind, struct_layout) {
        return Err(VenueError::UnknownVenue(format!(
            "{} is named like a {} but its layout is a different struct",
            short_tag(tag),
            kind.label()
        )));
    }
    Ok(match kind {
        VenueKind::CetusClmm => AnyVenue::Cetus(Box::new(CetusClmm::decode(bytes, layout)?)),
        VenueKind::VoloNativePool => {
            AnyVenue::Volo(Box::new(VoloNativePool::decode(bytes, layout)?))
        }
        VenueKind::NaviStorage => AnyVenue::Navi(Box::new(NaviStorage::decode(bytes, layout)?)),
    })
}

/// The fields a venue of each kind must have for the decoder to be the right one.
///
/// These are the fields the decoders require; listing them here means the check and the decode can
/// never disagree about what "this is the right struct" means.
const fn required_fields(kind: VenueKind) -> &'static [&'static str] {
    match kind {
        VenueKind::CetusClmm => &[
            "coin_a",
            "coin_b",
            "tick_spacing",
            "fee_rate",
            "liquidity",
            "current_sqrt_price",
            "current_tick_index",
            "tick_manager",
        ],
        VenueKind::VoloNativePool => &["pending", "collectable_fee", "validator_set"],
        VenueKind::NaviStorage => &["reserves", "reserves_count", "user_info"],
    }
}

/// True when `layout` has every field its venue kind requires.
#[must_use]
pub fn layout_has_shape(kind: VenueKind, layout: &MoveStructLayout) -> bool {
    required_fields(kind)
        .iter()
        .all(|required| layout.fields.iter().any(|field| field.name.as_str() == *required))
}

#[cfg(test)]
mod tests {
    use move_core_types::{
        account_address::AccountAddress, identifier::Identifier, language_storage::StructTag,
    };

    use super::{PriceState, VenueKind, venue_kind_of};

    /// The literals these tests use are valid Move identifiers by construction, and `Identifier`
    /// offers no infallible constructor; a failure here would be a bug in the test itself.
    fn ident(text: &str) -> Identifier {
        Identifier::new(text).unwrap_or_else(|_| unreachable!())
    }

    fn tag(module: &str, name: &str) -> StructTag {
        StructTag {
            address: AccountAddress::new([0x11; 32]),
            module: ident(module),
            name: ident(name),
            type_params: vec![],
        }
    }

    #[test]
    fn recognises_venues_by_module_and_name_not_by_package() {
        assert_eq!(venue_kind_of(&tag("pool", "Pool")), Some(VenueKind::CetusClmm));
        assert_eq!(
            venue_kind_of(&tag("native_pool", "NativePool")),
            Some(VenueKind::VoloNativePool)
        );
        assert_eq!(venue_kind_of(&tag("storage", "Storage")), Some(VenueKind::NaviStorage));
        assert_eq!(venue_kind_of(&tag("pool", "NotAPool")), None);
    }

    #[test]
    fn only_the_clmm_discovers_prices_on_chain() {
        assert!(VenueKind::CetusClmm.has_on_chain_price_discovery());
        assert!(!VenueKind::VoloNativePool.has_on_chain_price_discovery());
        assert!(!VenueKind::NaviStorage.has_on_chain_price_discovery());
    }

    #[test]
    fn price_state_converts_to_a_real_price() {
        // Mainnet pool A at the version transaction T read.
        let state = PriceState {
            sqrt_price: 647_308_812_393_509_050_120,
            liquidity: 120_115_891_674_982,
            tick: 71_162,
        };
        // √P/2^64 ≈ 35.0907, so P ≈ 1231.4 raw SUI per raw USDC — i.e. about 0.81 USDC per SUI.
        assert!((state.sqrt_price_real() - 35.090_681_033_302_715).abs() < 1e-9);
        assert!((state.price_raw() - 1_231.355_71).abs() < 0.01);
        let usdc_per_sui = 1_000_000_000.0 / state.price_raw() / 1_000_000.0;
        assert!((usdc_per_sui - 0.812_1).abs() < 0.001);
    }
}
