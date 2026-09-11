//! Tests that feed the decoders **real BCS bytes**, rather than building values directly.
//!
//! Everything else in this crate's tests constructs `TickNode` values by hand, which left the
//! decoders that actually read a pool's tick skip list off chain untested — a mutation that deleted
//! any match arm from any decoder survived. These tests serialise a struct with `bcs` against a
//! hand-built `MoveStructLayout` of the same shape the resolver produces, and decode it back.

use std::error::Error;

use birdai_move::{I32, I128, decode_struct};
use move_core_types::{
    account_address::AccountAddress,
    annotated_value::{MoveFieldLayout, MoveStructLayout, MoveTypeLayout},
    identifier::Identifier,
    language_storage::StructTag,
};
use serde::Serialize;
use sui_types::base_types::ObjectID;

use crate::{
    Tick,
    index::{FieldNodeDecoder, SkipListDecoder, TickDecoder, TickManagerDecoder, TickNodeDecoder},
};

/// Every test here builds inputs that are valid by construction, so failures carry a message.
type TestResult = Result<(), Box<dyn Error>>;

// The on-chain shapes, in serialisation order.

#[derive(Serialize)]
struct RawI32 {
    bits: u32,
}

#[derive(Serialize)]
struct RawI128 {
    bits: u128,
}

#[derive(Serialize)]
struct RawOptionU64 {
    is_none: bool,
    v: u64,
}

#[derive(Serialize)]
struct RawTick {
    index: RawI32,
    sqrt_price: u128,
    liquidity_net: RawI128,
    liquidity_gross: u128,
    fee_growth_outside_a: u128,
    fee_growth_outside_b: u128,
    points_growth_outside: u128,
    rewards_growth_outside: Vec<u128>,
}

#[derive(Serialize)]
struct RawNode {
    score: u64,
    nexts: Vec<RawOptionU64>,
    prev: RawOptionU64,
    value: RawTick,
}

#[derive(Serialize)]
struct RawId {
    bytes: AccountAddress,
}

#[derive(Serialize)]
struct RawUid {
    id: RawId,
}

#[derive(Serialize)]
struct RawField {
    id: RawUid,
    name: u64,
    value: RawNode,
}

#[derive(Serialize)]
struct RawRandom {
    seed: u64,
}

#[derive(Serialize)]
struct RawSkipList {
    id: RawUid,
    head: Vec<RawOptionU64>,
    tail: RawOptionU64,
    level: u64,
    max_level: u64,
    /// Present on the wire but not read; exercising this is the point — a decoder that forgot to
    /// skip it would read every later field from the wrong offset.
    list_p: u64,
    size: u64,
    random: RawRandom,
}

#[derive(Serialize)]
struct RawTickManager {
    tick_spacing: u32,
    ticks: RawSkipList,
}

fn field(name: &str, layout: MoveTypeLayout) -> Result<MoveFieldLayout, Box<dyn Error>> {
    Ok(MoveFieldLayout::new(Identifier::new(name)?, layout))
}

fn layout(
    module: &str,
    name: &str,
    fields: Vec<MoveFieldLayout>,
) -> Result<MoveStructLayout, Box<dyn Error>> {
    Ok(MoveStructLayout {
        type_: StructTag {
            address: AccountAddress::new([0x11; 32]),
            module: Identifier::new(module)?,
            name: Identifier::new(name)?,
            type_params: vec![],
        },
        fields,
    })
}

fn struct_of(layout: MoveTypeLayout, what: &str) -> Result<MoveStructLayout, Box<dyn Error>> {
    match layout {
        MoveTypeLayout::Struct(inner) => Ok(*inner),
        _ => Err(format!("{what} should be a struct").into()),
    }
}

fn i32_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "i32",
        "I32",
        vec![field("bits", MoveTypeLayout::U32)?],
    )?)))
}

fn i128_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "i128",
        "I128",
        vec![field("bits", MoveTypeLayout::U128)?],
    )?)))
}

fn option_u64_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "option_u64",
        "OptionU64",
        vec![field("is_none", MoveTypeLayout::Bool)?, field("v", MoveTypeLayout::U64)?],
    )?)))
}

fn tick_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "tick",
        "Tick",
        vec![
            field("index", i32_layout()?)?,
            field("sqrt_price", MoveTypeLayout::U128)?,
            field("liquidity_net", i128_layout()?)?,
            field("liquidity_gross", MoveTypeLayout::U128)?,
            field("fee_growth_outside_a", MoveTypeLayout::U128)?,
            field("fee_growth_outside_b", MoveTypeLayout::U128)?,
            field("points_growth_outside", MoveTypeLayout::U128)?,
            field(
                "rewards_growth_outside",
                MoveTypeLayout::Vector(Box::new(MoveTypeLayout::U128)),
            )?,
        ],
    )?)))
}

fn node_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "skip_list",
        "Node",
        vec![
            field("score", MoveTypeLayout::U64)?,
            field("nexts", MoveTypeLayout::Vector(Box::new(option_u64_layout()?)))?,
            field("prev", option_u64_layout()?)?,
            field("value", tick_layout()?)?,
        ],
    )?)))
}

fn uid_layout() -> Result<MoveTypeLayout, Box<dyn Error>> {
    Ok(MoveTypeLayout::Struct(Box::new(layout(
        "object",
        "UID",
        vec![field(
            "id",
            MoveTypeLayout::Struct(Box::new(layout(
                "object",
                "ID",
                vec![field("bytes", MoveTypeLayout::Address)?],
            )?)),
        )?],
    )?)))
}

fn skip_list_layout() -> Result<MoveStructLayout, Box<dyn Error>> {
    layout(
        "skip_list",
        "SkipList",
        vec![
            field("id", uid_layout()?)?,
            field("head", MoveTypeLayout::Vector(Box::new(option_u64_layout()?)))?,
            field("tail", option_u64_layout()?)?,
            field("level", MoveTypeLayout::U64)?,
            field("max_level", MoveTypeLayout::U64)?,
            field("list_p", MoveTypeLayout::U64)?,
            field("size", MoveTypeLayout::U64)?,
            field(
                "random",
                MoveTypeLayout::Struct(Box::new(layout(
                    "random",
                    "Random",
                    vec![field("seed", MoveTypeLayout::U64)?],
                )?)),
            )?,
        ],
    )
}

fn sample_tick(liquidity_net_bits: u128) -> RawTick {
    RawTick {
        index: RawI32 { bits: 71_180 },
        sqrt_price: 647_882_882_935_015_212_980,
        liquidity_net: RawI128 { bits: liquidity_net_bits },
        liquidity_gross: 9_000_000_000_000,
        fee_growth_outside_a: 12_345_678,
        fee_growth_outside_b: 87_654_321,
        points_growth_outside: 42,
        rewards_growth_outside: vec![1, 2, 3],
    }
}

#[test]
fn decodes_a_tick() -> TestResult {
    let raw = sample_tick(212_759_778_363);
    let bytes = bcs::to_bytes(&raw)?;
    let tick = decode_struct(&bytes, &struct_of(tick_layout()?, "a tick")?, TickDecoder)?;

    assert_eq!(tick.index, I32::new(71_180));
    assert_eq!(tick.sqrt_price, 647_882_882_935_015_212_980);
    assert_eq!(tick.liquidity_net, I128::new(212_759_778_363));
    assert_eq!(tick.liquidity_gross, 9_000_000_000_000);
    assert_eq!(tick.fee_growth_outside_a, 12_345_678);
    assert_eq!(tick.fee_growth_outside_b, 87_654_321);
    assert_eq!(tick.points_growth_outside, 42);
    assert_eq!(tick.rewards_growth_outside, vec![1, 2, 3]);
    Ok(())
}

#[test]
fn decodes_a_negative_liquidity_net() -> TestResult {
    // Two's complement: all ones is -1.
    let raw = sample_tick(u128::MAX);
    let bytes = bcs::to_bytes(&raw)?;
    let tick = decode_struct(&bytes, &struct_of(tick_layout()?, "a tick")?, TickDecoder)?;
    assert_eq!(tick.liquidity_net, I128::new(-1));
    Ok(())
}

#[test]
fn decodes_a_skip_list_node() -> TestResult {
    let raw = RawNode {
        score: 514_816,
        nexts: vec![
            RawOptionU64 { is_none: false, v: 514_826 },
            RawOptionU64 { is_none: true, v: 0 },
        ],
        prev: RawOptionU64 { is_none: false, v: 514_686 },
        value: sample_tick(212_759_778_363),
    };
    let bytes = bcs::to_bytes(&raw)?;
    let node = decode_struct(&bytes, &struct_of(node_layout()?, "a node")?, TickNodeDecoder)?;

    assert_eq!(node.score, 514_816);
    assert_eq!(node.index(), 71_180);
    // `nexts` drops the empty link, which is what a walk over the list expects.
    assert_eq!(node.nexts, vec![514_826]);
    assert_eq!(node.prev, Some(514_686));
    assert_eq!(node.tick.liquidity_net, I128::new(212_759_778_363));
    Ok(())
}

#[test]
fn decodes_a_dynamic_field_wrapping_a_node() -> TestResult {
    let raw = RawField {
        id: RawUid { id: RawId { bytes: AccountAddress::new([0x22; 32]) } },
        name: 514_816,
        value: RawNode {
            score: 514_816,
            nexts: vec![],
            prev: RawOptionU64 { is_none: true, v: 7 },
            value: sample_tick(1),
        },
    };
    let bytes = bcs::to_bytes(&raw)?;
    let field_layout = layout(
        "dynamic_field",
        "Field",
        vec![
            field("id", uid_layout()?)?,
            field("name", MoveTypeLayout::U64)?,
            field("value", node_layout()?)?,
        ],
    )?;
    let node = decode_struct(&bytes, &field_layout, FieldNodeDecoder)?;
    assert_eq!(node.score, 514_816);
    assert_eq!(node.index(), 71_180);
    // An `OptionU64` flagged empty keeps its payload on the wire but reads as `None`.
    assert_eq!(node.prev, None);
    Ok(())
}

#[test]
fn decodes_skip_list_metadata_including_the_inner_uid() -> TestResult {
    let raw = RawSkipList {
        id: RawUid { id: RawId { bytes: AccountAddress::new([0x7f; 32]) } },
        head: vec![RawOptionU64 { is_none: false, v: 514_686 }],
        tail: RawOptionU64 { is_none: false, v: 515_646 },
        level: 3,
        max_level: 16,
        list_p: 2,
        size: 648,
        random: RawRandom { seed: 9_876 },
    };
    let bytes = bcs::to_bytes(&raw)?;
    let head = decode_struct(&bytes, &skip_list_layout()?, SkipListDecoder)?;

    assert_eq!(head.node_uid, ObjectID::from(AccountAddress::new([0x7f; 32])));
    assert_eq!(head.head, Some(514_686));
    assert_eq!(head.tail, Some(515_646));
    assert_eq!(head.level, 3);
    assert_eq!(head.max_level, 16);
    assert_eq!(head.size, 648);
    assert_eq!(head.seed, 9_876);
    Ok(())
}

#[test]
fn decodes_a_tick_manager() -> TestResult {
    let raw = RawTickManager {
        tick_spacing: 10,
        ticks: RawSkipList {
            id: RawUid { id: RawId { bytes: AccountAddress::new([0x7f; 32]) } },
            head: vec![],
            tail: RawOptionU64 { is_none: true, v: 0 },
            level: 1,
            max_level: 16,
            list_p: 2,
            size: 1,
            random: RawRandom { seed: 1 },
        },
    };
    let bytes = bcs::to_bytes(&raw)?;
    let manager_layout = layout(
        "tick",
        "TickManager",
        vec![
            field("tick_spacing", MoveTypeLayout::U32)?,
            field("ticks", MoveTypeLayout::Struct(Box::new(skip_list_layout()?)))?,
        ],
    )?;
    let head = decode_struct(&bytes, &manager_layout, TickManagerDecoder)?;
    assert_eq!(head.size, 1);
    assert_eq!(head.head, None, "an empty head vector reads as no head");
    Ok(())
}

#[test]
fn a_missing_required_field_is_an_error() -> TestResult {
    // A layout without `sqrt_price` must not silently produce zero.
    let raw = sample_tick(1);
    let bytes = bcs::to_bytes(&raw)?;
    let trimmed = layout("tick", "Tick", vec![field("index", i32_layout()?)?])?;
    let outcome: Result<Tick, _> = decode_struct(&bytes, &trimmed, TickDecoder);
    assert!(outcome.is_err(), "a missing required field must fail loudly");
    Ok(())
}
