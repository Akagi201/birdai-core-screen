//! Cetus concentrated-liquidity pool: `…::pool::Pool<CoinTypeA, CoinTypeB>`.
//!
//! Field order, verified against mainnet pool A (`770` bytes of BCS):
//!
//! ```text
//! id, coin_a, coin_b, tick_spacing, fee_rate, liquidity, current_sqrt_price,
//! current_tick_index, fee_growth_global_a, fee_growth_global_b, fee_protocol_coin_a,
//! fee_protocol_coin_b, tick_manager, rewarder_manager, position_manager, is_pause, index, url
//! ```
//!
//! Three details are easy to get wrong and are handled here:
//!
//! * both type parameters are `phantom`, so they occupy **zero** BCS bytes and the layout is
//!   identical for every `Pool<X, Y>` — only the tag differs;
//! * `current_tick_index` is Cetus's own `I32`, a `u32` of two's-complement bits rather than a
//!   signed integer on the wire;
//! * `tick_manager.ticks` is a skip list **inlined** in the pool whose `UID` is not the object's,
//!   so its nodes are dynamic fields of an inner UID and never show up as the pool's children.

use birdai_amm::{Direction, MAX_SQRT_PRICE, MIN_SQRT_PRICE, PoolState, SwapResult, swap_exact_in};
use birdai_move::{
    DecodeError, StructDecoder, bytes_field, dump::short_type_of, required, struct_field,
    u32_field, u64_field, u128_field,
};
use birdai_tick::{SkipListDecoder, SkipListHead, Ticks};
use move_core_types::{annotated_value::MoveTypeLayout, annotated_visitor::StructDriver};

use crate::{
    error::VenueError,
    venue::{PriceState, Venue, VenueKind},
};

/// Module that defines the pool type.
pub const CETUS_POOL_MODULE: &str = "pool";

/// Name of the pool type.
pub const CETUS_POOL_NAME: &str = "Pool";

/// How many tick boundaries a single quote is allowed to cross.
///
/// A pool with a corrupted tick index would otherwise turn one quote into an unbounded walk.
pub const MAX_TICK_CROSSINGS: u32 = 64;

/// A Cetus concentrated-liquidity pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CetusClmm {
    /// `coin_a` reserve, in raw units.
    pub coin_a: u64,
    /// `coin_b` reserve, in raw units.
    pub coin_b: u64,
    /// Minimum tick-index distance between initialised ticks.
    pub tick_spacing: u32,
    /// Fee in millionths: `500` is 5 bps.
    pub fee_rate: u64,
    /// Active liquidity `L`.
    pub liquidity: u128,
    /// Square-root price in Q64.64.
    pub sqrt_price: u128,
    /// Current tick index.
    pub tick: i32,
    /// Cumulative fee growth per unit of liquidity on side A.
    pub fee_growth_global_a: u128,
    /// Cumulative fee growth per unit of liquidity on side B.
    pub fee_growth_global_b: u128,
    /// Protocol's share of token A fees.
    pub fee_protocol_coin_a: u64,
    /// Protocol's share of token B fees.
    pub fee_protocol_coin_b: u64,
    /// The tick skip list's metadata. Its nodes are separate dynamic-field objects.
    pub ticks: SkipListHead,
    /// True when swaps are halted.
    pub is_paused: bool,
    /// The pool's registration index.
    pub index: u64,
    /// The pool's metadata URL, as raw `0x1::string::String` bytes.
    pub url: Vec<u8>,
}

impl CetusClmm {
    /// True when the pool can be priced at all.
    #[must_use]
    pub fn is_priceable(&self) -> bool {
        !self.is_paused &&
            self.liquidity > 0 &&
            self.sqrt_price > 0 &&
            (birdai_amm::MIN_SQRT_PRICE..=birdai_amm::MAX_SQRT_PRICE).contains(&self.sqrt_price) &&
            self.fee_rate < birdai_amm::FEE_DENOMINATOR
    }

    /// The pool's metadata URL as text, lossily if it is not UTF-8.
    #[must_use]
    pub fn url_string(&self) -> String {
        String::from_utf8_lossy(&self.url).into_owned()
    }

    /// Price of one raw unit of B in raw units of A, as a real number. Display only.
    #[must_use]
    pub fn price_raw(&self) -> Option<f64> {
        self.price_state().map(|state| state.price_raw())
    }

    /// Quote an exact-input swap, walking tick boundaries.
    ///
    /// `ticks` supplies the initialised ticks; without them the quote is computed as if the price
    /// never crosses a boundary, which for a small trade against a deep pool is exact.
    pub fn quote_exact_in(
        &self,
        ticks: &Ticks,
        direction: Direction,
        amount_in: u128,
    ) -> Result<SwapResult, birdai_amm::AmmError> {
        let limit = match direction {
            Direction::BtoA => MAX_SQRT_PRICE,
            Direction::AtoB => MIN_SQRT_PRICE,
        };
        swap_exact_in(ticks, direction, self.state(), amount_in, limit, MAX_TICK_CROSSINGS)
    }

    /// The pool's swap-relevant state, as the arithmetic crate wants it.
    #[must_use]
    pub const fn state(&self) -> PoolState {
        PoolState {
            sqrt_price: self.sqrt_price,
            liquidity: self.liquidity,
            tick: self.tick,
            fee_rate: self.fee_rate,
        }
    }

    /// True when a quote of `amount_in` would stay inside the tick the pool is in right now.
    ///
    /// This is the check that makes a single-step quote exact without consulting the tick index.
    /// Initialised ticks sit on multiples of `tick_spacing` and the pool's stored tick is the floor
    /// tick of its current price, so if the reached price floors to the *same* tick then no
    /// integer tick — and therefore no boundary — lies between the two prices.
    ///
    /// `tick_spacing` on its own is not enough: from tick 71 162 the next multiple of 10 is 71 170,
    /// eight ticks away, so a move of nine ticks would cross a boundary that a "less than
    /// `tick_spacing`" test calls safe.
    pub fn stays_inside_current_range(
        &self,
        direction: Direction,
        amount_in: u128,
    ) -> Result<bool, birdai_amm::AmmError> {
        if self.liquidity == 0 {
            return Ok(false);
        }
        let (net, _fee) = birdai_amm::take_fee(amount_in, self.fee_rate)?;
        let reached = match direction {
            Direction::BtoA => {
                birdai_amm::next_sqrt_price_up(self.sqrt_price, self.liquidity, net)?
            }
            Direction::AtoB => {
                birdai_amm::next_sqrt_price_down(self.sqrt_price, self.liquidity, net)?
            }
        };
        // The argument needs the stored tick to be the floor tick of the stored price. A pool whose
        // bookkeeping disagrees with its price gets no shortcut: the tick index has to be used.
        let start = birdai_amm::tick_at_sqrt_price(self.sqrt_price)?;
        if start != self.tick {
            return Ok(false);
        }
        Ok(birdai_amm::tick_at_sqrt_price(reached)? == start)
    }

    /// The first initialised tick at or above `self.tick`, as the spacing grid implies it.
    ///
    /// Every initialised tick is a multiple of `tick_spacing`, so this is the nearest boundary the
    /// price can meet — without enumerating the skip list, and therefore without reading the tick
    /// set at a version it cannot be read at.
    #[must_use]
    pub const fn next_grid_tick(&self) -> i32 {
        let spacing = self.tick_spacing as i32;
        if spacing <= 0 {
            return self.tick;
        }
        // Euclidean division, so this holds for negative ticks too.
        let floor = self.tick.div_euclid(spacing);
        floor.saturating_add(1).saturating_mul(spacing)
    }
}

impl Venue for CetusClmm {
    const KIND: VenueKind = VenueKind::CetusClmm;

    fn decode(bytes: &[u8], layout: &MoveTypeLayout) -> Result<Self, VenueError> {
        let MoveTypeLayout::Struct(struct_layout) = layout else {
            return Err(VenueError::NotAStruct { tag: short_type_of(layout) });
        };
        Ok(birdai_move::decode_struct(bytes, struct_layout, CetusPoolDecoder)?)
    }

    fn price_state(&self) -> Option<PriceState> {
        Some(PriceState { sqrt_price: self.sqrt_price, liquidity: self.liquidity, tick: self.tick })
    }
}

/// Decoder for the pool, matching fields by name so an upgrade that appends a field is harmless.
#[derive(Debug, Default, Clone, Copy)]
pub struct CetusPoolDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for CetusPoolDecoder {
    type Output = CetusClmm;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut coin_a = None;
        let mut coin_b = None;
        let mut tick_spacing = None;
        let mut fee_rate = None;
        let mut liquidity = None;
        let mut sqrt_price = None;
        let mut tick = None;
        let mut fee_growth_global_a = None;
        let mut fee_growth_global_b = None;
        let mut fee_protocol_coin_a = None;
        let mut fee_protocol_coin_b = None;
        let mut ticks = None;
        let mut is_paused = None;
        let mut index = None;
        let mut url = None;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "coin_a" => coin_a = Some(struct_field(driver, BalanceDecoder)?),
                "coin_b" => coin_b = Some(struct_field(driver, BalanceDecoder)?),
                "tick_spacing" => tick_spacing = Some(u32_field(driver)?),
                "fee_rate" => fee_rate = Some(u64_field(driver)?),
                "liquidity" => liquidity = Some(u128_field(driver)?),
                "current_sqrt_price" => sqrt_price = Some(u128_field(driver)?),
                "current_tick_index" => {
                    tick = Some(struct_field(driver, birdai_move::I32Decoder)?.get());
                }
                "fee_growth_global_a" => fee_growth_global_a = Some(u128_field(driver)?),
                "fee_growth_global_b" => fee_growth_global_b = Some(u128_field(driver)?),
                "fee_protocol_coin_a" => fee_protocol_coin_a = Some(u64_field(driver)?),
                "fee_protocol_coin_b" => fee_protocol_coin_b = Some(u64_field(driver)?),
                "tick_manager" => ticks = Some(struct_field(driver, TickManagerProxy)?),
                "is_pause" => is_paused = Some(birdai_move::bool_field(driver)?),
                "index" => index = Some(u64_field(driver)?),
                "url" => url = Some(struct_field(driver, MoveStringDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(CetusClmm {
            coin_a: required(coin_a, "coin_a")?,
            coin_b: required(coin_b, "coin_b")?,
            tick_spacing: required(tick_spacing, "tick_spacing")?,
            fee_rate: required(fee_rate, "fee_rate")?,
            liquidity: required(liquidity, "liquidity")?,
            sqrt_price: required(sqrt_price, "current_sqrt_price")?,
            tick: required(tick, "current_tick_index")?,
            fee_growth_global_a: required(fee_growth_global_a, "fee_growth_global_a")?,
            fee_growth_global_b: required(fee_growth_global_b, "fee_growth_global_b")?,
            fee_protocol_coin_a: fee_protocol_coin_a.unwrap_or(0),
            fee_protocol_coin_b: fee_protocol_coin_b.unwrap_or(0),
            ticks: required(ticks, "tick_manager")?,
            // Required rather than defaulted: `is_priceable` gates on it, and a pool that halts
            // swaps while we price it as live is the one failure mode this field exists to stop.
            is_paused: required(is_paused, "is_pause")?,
            index: index.unwrap_or(0),
            url: url.unwrap_or_default(),
        })
    }
}

/// Reads `0x2::balance::Balance<T> { value: u64 }`.
///
/// `Balance` is inlined: it is a `u64` one level down, never a child object.
#[derive(Debug, Default, Clone, Copy)]
pub struct BalanceDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for BalanceDecoder {
    type Output = u64;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "value" => value = Some(u64_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(value, "value")
    }
}

/// Reads `0x1::string::String { bytes: vector<u8> }` and `0x1::ascii::String` alike.
#[derive(Debug, Default, Clone, Copy)]
pub struct MoveStringDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for MoveStringDecoder {
    type Output = Vec<u8>;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut bytes = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bytes" => bytes = Some(bytes_field(driver)?.to_vec()),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(bytes, "bytes")
    }
}

/// Reads `tick::TickManager` and keeps only the tick skip list's metadata.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickManagerProxy;

impl<'b, 'l> StructDecoder<'b, 'l> for TickManagerProxy {
    type Output = SkipListHead;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut ticks = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "ticks" => ticks = Some(struct_field(driver, SkipListDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(ticks, "ticks")
    }
}
