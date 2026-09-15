//! Navi lending storage: `…::storage::Storage`.
//!
//! The most instructive of the three objects. Its whole BCS is 155 bytes and contains **no**
//! balances at all: `reserves` and `user_info` are `0x2::table::Table`s, so the object carries two
//! `UID`s and two lengths while the 35 reserves and ~999k user positions live in dynamic fields.
//!
//! It is not a trading venue, and the reason is not that it lacks a `Balance` field — it is that
//! asset prices inside it are *imported from an oracle* rather than discovered by trading, and
//! supplies and borrows are governed by a utilisation curve, which is an interest rate rather than
//! a price. [`crate::classify`] encodes that distinction.

use birdai_move::{DecodeError, StructDecoder, struct_field, u64_field};
use move_core_types::{
    account_address::AccountAddress, annotated_value::MoveTypeLayout,
    annotated_visitor::StructDriver,
};

use crate::{
    error::VenueError,
    venue::{PriceState, Venue, VenueKind},
    volo::TableSizeDecoder,
};

/// Module that defines the Navi storage type.
pub const NAVI_STORAGE_MODULE: &str = "storage";

/// Name of the Navi storage type.
pub const NAVI_STORAGE_NAME: &str = "Storage";

/// A Navi lending storage object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NaviStorage {
    /// Storage layout version, bumped when the pool's bookkeeping changes.
    pub version: u64,
    /// True when deposits and borrows are halted.
    pub paused: bool,
    /// Number of reserves, from `reserves.size`.
    pub reserves_size: u64,
    /// Number of reserves, as a separate `u8` field.
    pub reserves_count: u8,
    /// Addresses with a position.
    pub users: Vec<AccountAddress>,
    /// Number of user positions, from `user_info.size`.
    pub user_info_size: u64,
}

impl NaviStorage {
    /// Whether the object's two redundant reserve counters agree.
    ///
    /// They disagreeing is not an error by itself, but it is a cheap consistency signal: both come
    /// from the same object, so a mismatch means the layout was misread.
    #[must_use]
    pub fn reserve_counters_agree(&self) -> bool {
        u64::from(self.reserves_count) == self.reserves_size
    }
}

impl Venue for NaviStorage {
    const KIND: VenueKind = VenueKind::NaviStorage;

    fn decode(bytes: &[u8], layout: &MoveTypeLayout) -> Result<Self, VenueError> {
        let MoveTypeLayout::Struct(struct_layout) = layout else {
            return Err(VenueError::NotAStruct { tag: birdai_move::dump::short_type_of(layout) });
        };
        Ok(birdai_move::decode_struct(bytes, struct_layout, NaviStorageDecoder)?)
    }

    /// Always `None`: `Storage` holds no price of its own.
    ///
    /// Reserves are valued through an external oracle and interest is a function of utilisation,
    /// so nothing in this object's fields determines what one asset trades for in terms of another.
    fn price_state(&self) -> Option<PriceState> {
        None
    }
}

/// Decoder for the Navi storage object.
#[derive(Debug, Default, Clone, Copy)]
pub struct NaviStorageDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for NaviStorageDecoder {
    type Output = NaviStorage;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut version = 0_u64;
        let mut paused = false;
        let mut reserves_size = 0_u64;
        let mut reserves_count = 0_u8;
        let mut users = Vec::new();
        let mut user_info_size = 0_u64;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "version" => version = u64_field(driver)?,
                "paused" => paused = birdai_move::bool_field(driver)?,
                "reserves" => reserves_size = struct_field(driver, TableSizeDecoder)?,
                "reserves_count" => reserves_count = birdai_move::u8_field(driver)?,
                "users" => users = birdai_move::address_vec_field(driver)?,
                "user_info" => user_info_size = struct_field(driver, TableSizeDecoder)?,
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(NaviStorage { version, paused, reserves_size, reserves_count, users, user_info_size })
    }
}
