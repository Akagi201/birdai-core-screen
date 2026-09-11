//! Volo liquid-staking pool: `…::native_pool::NativePool`.
//!
//! Pool-shaped, and deliberately typed anyway — the interesting part of the classification task is
//! being able to say precisely *why* an object that holds a `Balance<SUI>` and looks like a pool is
//! not a trading venue. The state that matters is an accounting ratio (staked principal plus
//! rewards over shares), not a price that order flow moves.

use birdai_move::{
    DecodeError, StructDecoder, required, struct_field, struct_vec_field, u64_field,
};
use move_core_types::{
    account_address::AccountAddress, annotated_value::MoveTypeLayout,
    annotated_visitor::StructDriver,
};

use crate::{
    cetus::BalanceDecoder,
    error::VenueError,
    venue::{PriceState, Venue, VenueKind},
};

/// Module that defines the Volo pool type.
pub const VOLO_POOL_MODULE: &str = "native_pool";

/// Name of the Volo pool type.
pub const VOLO_POOL_NAME: &str = "NativePool";

/// A Volo liquid-staking pool.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoloNativePool {
    /// SUI awaiting the next validator-set rebalance, in MIST.
    pub pending: u64,
    /// Fees collected and not yet moved, in MIST.
    pub collectable_fee: u64,
    /// Number of validator vaults, from `validator_set.vaults.size`.
    pub vaults: u64,
    /// Validator address to stake weight, from `validator_set.validators`.
    pub validators: Vec<(AccountAddress, u64)>,
    /// Validators in the order the pool iterates them.
    pub sorted_validators: Vec<AccountAddress>,
}

impl VoloNativePool {
    /// Total SUI the object itself shows, in MIST.
    #[must_use]
    pub const fn visible_sui(&self) -> u64 {
        self.pending.saturating_add(self.collectable_fee)
    }
}

impl Venue for VoloNativePool {
    const KIND: VenueKind = VenueKind::VoloNativePool;

    fn decode(bytes: &[u8], layout: &MoveTypeLayout) -> Result<Self, VenueError> {
        let MoveTypeLayout::Struct(struct_layout) = layout else {
            return Err(VenueError::NotAStruct { tag: birdai_move::dump::short_type_of(layout) });
        };
        Ok(birdai_move::decode_struct(bytes, struct_layout, VoloPoolDecoder)?)
    }

    /// Always `None`.
    ///
    /// The SUI↔VSUI rate is `total_staked / total_shares` and moves with reward accrual and
    /// validator rebalancing, not with order flow. There is no marginal price to quote against.
    fn price_state(&self) -> Option<PriceState> {
        None
    }
}

/// Decoder for the Volo pool.
#[derive(Debug, Default, Clone, Copy)]
pub struct VoloPoolDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for VoloPoolDecoder {
    type Output = VoloNativePool;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut pending = None;
        let mut collectable_fee = None;
        let mut validator_set = None;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "pending" => pending = Some(struct_field(driver, BalanceHolderDecoder)?),
                "collectable_fee" => {
                    collectable_fee = Some(struct_field(driver, BalanceHolderDecoder)?);
                }
                "validator_set" => validator_set = Some(struct_field(driver, ValidatorSetDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        let validator_set = required(validator_set, "validator_set")?;
        Ok(VoloNativePool {
            pending: required(pending, "pending")?,
            collectable_fee: required(collectable_fee, "collectable_fee")?,
            vaults: validator_set.vaults,
            validators: validator_set.validators,
            sorted_validators: validator_set.sorted_validators,
        })
    }
}

/// The parts of `validator_set` that describe how much is staked where.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidatorSet {
    /// Number of vaults.
    pub vaults: u64,
    /// Validator weights.
    pub validators: Vec<(AccountAddress, u64)>,
    /// Iteration order.
    pub sorted_validators: Vec<AccountAddress>,
}

/// Reads `validator_set`: a `Table` (metadata only), a `VecMap` and a vector of addresses.
#[derive(Debug, Default, Clone, Copy)]
pub struct ValidatorSetDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for ValidatorSetDecoder {
    type Output = ValidatorSet;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut vaults = 0_u64;
        let mut validators = Vec::new();
        let mut sorted_validators = Vec::new();

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                // `Table<..>` is a dynamic-field container: the BCS holds `{ id, size }` and
                // nothing else, so `size` is metadata and never the contents.
                "vaults" => vaults = struct_field(driver, TableSizeDecoder)?,
                "validators" => validators = struct_field(driver, VecMapDecoder)?,
                "sorted_validators" => {
                    sorted_validators = birdai_move::address_vec_field(driver)?;
                }
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(ValidatorSet { vaults, validators, sorted_validators })
    }
}

/// Reads a `{ id: UID, balance: Balance<T> }` pair, keeping the balance.
#[derive(Debug, Default, Clone, Copy)]
pub struct BalanceHolderDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for BalanceHolderDecoder {
    type Output = u64;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut balance = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "balance" => balance = Some(struct_field(driver, BalanceDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(balance, "balance")
    }
}

/// Reads `0x2::table::Table { id: UID, size: u64 }`, keeping the size.
#[derive(Debug, Default, Clone, Copy)]
pub struct TableSizeDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for TableSizeDecoder {
    type Output = u64;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut size = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "size" => size = Some(u64_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(size, "size")
    }
}

/// Reads `0x2::vec_map::VecMap<address, u64>` into pairs.
#[derive(Debug, Default, Clone, Copy)]
pub struct VecMapDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for VecMapDecoder {
    type Output = Vec<(AccountAddress, u64)>;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut contents = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "contents" => contents = Some(struct_vec_field(driver, VecMapEntryDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        Ok(contents.unwrap_or_default())
    }
}

/// Reads one `0x2::vec_map::Entry<address, u64>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct VecMapEntryDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for VecMapEntryDecoder {
    type Output = (AccountAddress, u64);

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut key = None;
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "key" => key = Some(birdai_move::address_field(driver)?),
                "value" => value = Some(u64_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        Ok((required(key, "key")?, required(value, "value")?))
    }
}
