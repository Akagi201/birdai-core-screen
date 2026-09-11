//! Decoding and indexing of the pool's tick skip list.

use std::collections::{BTreeMap, HashMap};

use birdai_amm::{Boundary, TickSource, sqrt_price_at_tick};
use birdai_move::{
    DecodeError, I32, I128, OptionU64, OptionU64Decoder, StructDecoder, required, struct_field,
    struct_vec_field, u64_field, u128_field, u128_vec_field,
};
use move_core_types::{account_address::AccountAddress, annotated_visitor::StructDriver};
use sui_types::base_types::ObjectID;

use crate::error::TickError;

/// Cetus's `MAX_TICK`, used as the bias that turns a signed tick into an orderable `u64` key.
pub const CETUS_TICK_BIAS: i32 = 443_636;

/// The skip-list key for a tick index: `tick + MAX_TICK`.
#[must_use]
pub const fn score_from_tick(tick: i32) -> u64 {
    (tick + CETUS_TICK_BIAS) as u64
}

/// The tick index behind a skip-list key.
#[must_use]
pub const fn tick_from_score(score: u64) -> i32 {
    score as i32 - CETUS_TICK_BIAS
}

/// The UID that owns the tick nodes, extracted from `tick_manager.ticks.id`.
///
/// Dynamic fields are attached to *this* id, not to the pool: enumerating the pool object's own
/// dynamic fields returns nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeUid(pub ObjectID);

/// One initialised tick, as stored in the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// The tick index. 71_162 is not a multiple of the pool's spacing; only *initialised* ticks
    /// sit on the spacing grid.
    pub index: I32,
    /// `⌊1.0001^(index/2) · 2^64⌋`, the price the tick switches at.
    pub sqrt_price: u128,
    /// Liquidity added to the active range when the price crosses this tick upwards.
    pub liquidity_net: I128,
    /// Total liquidity referencing this tick.
    pub liquidity_gross: u128,
    /// Fee growth per unit of liquidity on side A, below this tick.
    pub fee_growth_outside_a: u128,
    /// Fee growth per unit of liquidity on side B, below this tick.
    pub fee_growth_outside_b: u128,
    /// Cetus points-programme growth below this tick.
    pub points_growth_outside: u128,
    /// Reward growths below this tick, one entry per active rewarder.
    pub rewards_growth_outside: Vec<u128>,
}

/// A node of the skip list: the tick plus the list's own links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickNode {
    /// The node's key, equal to `score_from_tick(tick.index)`.
    pub score: u64,
    /// Forward links, one per level.
    pub nexts: Vec<u64>,
    /// Backward link, if any.
    pub prev: Option<u64>,
    /// The tick this node stores.
    pub tick: Tick,
}

impl TickNode {
    /// The tick index.
    #[must_use]
    pub const fn index(&self) -> i32 {
        self.tick.index.get()
    }
}

/// The skip list's own metadata, decoded from `tick_manager.ticks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipListHead {
    /// The UID whose dynamic fields are the nodes.
    pub node_uid: ObjectID,
    /// Level-0 head pointer as recorded in the metadata.
    pub head: Option<u64>,
    /// Tail pointer.
    pub tail: Option<u64>,
    /// Current level.
    pub level: u64,
    /// Maximum level.
    pub max_level: u64,
    /// Number of nodes the list believes it has.
    pub size: u64,
    /// The list's PRNG seed, which determines the level distribution.
    pub seed: u64,
}

impl SkipListHead {
    /// The UID the nodes hang off, as an [`ObjectID`].
    #[must_use]
    pub const fn node_uid(&self) -> ObjectID {
        self.node_uid
    }
}

/// How far a tick index's contents differ from the count its metadata declares.
///
/// A non-zero skew means the index was assembled from children read at a **different version** from
/// the metadata it is being compared against. That is unavoidable over RPC: dynamic fields are
/// enumerated as of now, while a pool object can be fetched at any historical version. It is
/// reported rather than swallowed because it is also exactly the signal a state manager needs — the
/// tick set of a pool that has been traded since is not the tick set the pool's metadata describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeSkew {
    /// The count the pool's skip-list metadata declares.
    pub declared: u64,
    /// The number of nodes actually observed.
    pub observed: u64,
}

impl SizeSkew {
    /// Nodes observed minus nodes declared; positive means children appeared after the metadata was
    /// written.
    #[must_use]
    pub const fn delta(self) -> i64 {
        self.observed as i64 - self.declared as i64
    }
}

/// How to find a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locate {
    /// Exactly this tick index.
    Exact(i32),
    /// The initialised tick nearest to this index, preferring the lower one on a tie.
    Nearest(i32),
}

/// A pool's tick index: every initialised tick, ordered and validated.
///
/// Built from decoded nodes plus the skip list metadata. Construction enforces the two invariants
/// that matter for pricing:
///
/// * the level-0 chain visits every node exactly once and is ordered by tick index — this is the
///   check that catches a partial or duplicated page walk, and
/// * every node's stored `sqrt_price` equals [`sqrt_price_at_tick`] of its index — this is the
///   check that catches a wrong tick math or a misread node.
#[derive(Debug, Clone, Default)]
pub struct Ticks {
    /// The UID the nodes were read from.
    node_uid: Option<ObjectID>,
    /// Nodes in ascending tick order.
    nodes: Vec<TickNode>,
    /// Tick index to position in `nodes`.
    by_tick: BTreeMap<i32, usize>,
    /// Skip-list key to position in `nodes`.
    by_score: HashMap<u64, usize>,
    /// The count the skip-list metadata declares. When `strict_size` is set, a mismatch is an
    /// error.
    declared_size: Option<u64>,
    /// Whether [`Ticks::validate`] should reject a declared-size mismatch.
    strict_size: bool,
    /// Histogram of `stored - computed` square-root prices, filled in by [`Ticks::validate`].
    price_deviations: BTreeMap<i128, usize>,
}

impl Ticks {
    /// Build an index from decoded nodes, requiring the skip list's declared `size` to match.
    ///
    /// Use this when the pool object and its children were read from the same state — a live pool,
    /// or a checkpoint replay. A mismatch then means the read was incomplete, which must not be
    /// papered over.
    pub fn new(
        node_uid: Option<ObjectID>,
        declared_size: Option<u64>,
        nodes: impl IntoIterator<Item = TickNode>,
    ) -> Result<Self, TickError> {
        let mut index = Self {
            node_uid,
            nodes: Vec::new(),
            by_tick: BTreeMap::new(),
            by_score: HashMap::new(),
            declared_size,
            strict_size: true,
            price_deviations: BTreeMap::new(),
        };
        index.fill(nodes)?;
        Ok(index)
    }

    /// Build an index from children read at a version that may not match the metadata.
    ///
    /// The size check is downgraded to a reported [`SizeSkew`]; every other invariant — node
    /// ordering, link integrity, and the tick math against each node's stored square-root price —
    /// is still enforced, because those do not depend on how many nodes there are.
    pub fn from_children(
        node_uid: Option<ObjectID>,
        declared_size: u64,
        nodes: impl IntoIterator<Item = TickNode>,
    ) -> Result<(Self, Option<SizeSkew>), TickError> {
        let mut index = Self {
            node_uid,
            nodes: Vec::new(),
            by_tick: BTreeMap::new(),
            by_score: HashMap::new(),
            declared_size: Some(declared_size),
            strict_size: false,
            price_deviations: BTreeMap::new(),
        };
        index.fill(nodes)?;
        let observed = index.nodes.len() as u64;
        let skew =
            (observed != declared_size).then_some(SizeSkew { declared: declared_size, observed });
        Ok((index, skew))
    }

    fn fill(&mut self, nodes: impl IntoIterator<Item = TickNode>) -> Result<(), TickError> {
        for node in nodes {
            self.insert(node)?;
        }
        self.validate()
    }

    /// The count the skip-list metadata declared, if one was supplied.
    #[must_use]
    pub const fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }

    /// True when the node count is known to match the declared size.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        match self.declared_size {
            Some(declared) => declared == self.nodes.len() as u64,
            None => true,
        }
    }

    /// Add one node.
    pub fn insert(&mut self, node: TickNode) -> Result<(), TickError> {
        if self.by_score.contains_key(&node.score) {
            return Err(TickError::DuplicateNode(node.score));
        }
        let position = self.nodes.len();
        self.by_score.insert(node.score, position);
        self.by_tick.insert(node.index(), position);
        self.nodes.push(node);
        Ok(())
    }

    /// Check the index against the skip list's own metadata and the tick math.
    pub fn validate(&mut self) -> Result<(), TickError> {
        // Keep `nodes` in tick order so iteration and neighbour lookup agree.
        self.nodes.sort_by_key(TickNode::index);
        self.by_tick.clear();
        self.by_score.clear();
        let mut deviations: BTreeMap<i128, usize> = BTreeMap::new();
        for (position, node) in self.nodes.iter().enumerate() {
            self.by_tick.insert(node.index(), position);
            self.by_score.insert(node.score, position);
        }

        if self.strict_size &&
            let Some(expected) = self.declared_size &&
            expected != self.nodes.len() as u64
        {
            return Err(TickError::SizeMismatch { expected, actual: self.nodes.len() });
        }

        for (position, node) in self.nodes.iter().enumerate() {
            if let Some(next) = node.nexts.first() {
                // A forward link must resolve to a *different* node that is present. Pointing at
                // itself, or at a node that was not decoded, means the walk is broken.
                let resolvable = self
                    .by_score
                    .get(next)
                    .is_some_and(|position_of_next| *position_of_next != position);
                if !resolvable {
                    return Err(TickError::DanglingLink { score: node.score, neighbour: *next });
                }
            }

            let computed = sqrt_price_at_tick(node.index())
                .map_err(|_| TickError::TickOutOfRange(node.index()))?;
            // Prices are ~2^64, so the signed difference always fits; the saturating conversions
            // only exist to avoid an infallible cast.
            let deviation = if node.tick.sqrt_price >= computed {
                i128::try_from(node.tick.sqrt_price - computed).unwrap_or(i128::MAX)
            } else {
                -i128::try_from(computed - node.tick.sqrt_price).unwrap_or(i128::MAX)
            };
            let tolerance = birdai_amm::tick_price_tolerance(computed);
            if deviation.unsigned_abs() > tolerance {
                return Err(TickError::PriceMismatch {
                    tick: node.index(),
                    stored: node.tick.sqrt_price,
                    computed,
                    deviation,
                    tolerance,
                });
            }
            deviations.entry(deviation).and_modify(|count| *count += 1).or_insert(1);
        }

        self.price_deviations = deviations;
        Ok(())
    }

    /// How many nodes sit at each deviation from [`birdai_amm::sqrt_price_at_tick`].
    ///
    /// A map from `stored - computed` to the number of ticks at that deviation. On mainnet most
    /// ticks land on `0` and a minority on `-1`; the distribution is exposed so the claim is
    /// measured rather than asserted.
    #[must_use]
    pub const fn price_deviations(&self) -> &BTreeMap<i128, usize> {
        &self.price_deviations
    }

    /// The largest absolute deviation of any node's stored price from the tick math.
    #[must_use]
    pub fn max_price_deviation(&self) -> u128 {
        self.price_deviations.keys().map(|deviation| deviation.unsigned_abs()).max().unwrap_or(0)
    }

    /// How many nodes store exactly the price the tick math derives.
    #[must_use]
    pub fn exact_price_nodes(&self) -> usize {
        self.price_deviations.get(&0).copied().unwrap_or(0)
    }

    /// Number of initialised ticks.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the pool has no initialised ticks.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The UID the nodes live under.
    #[must_use]
    pub const fn node_uid(&self) -> Option<ObjectID> {
        self.node_uid
    }

    /// The smallest initialised tick.
    #[must_use]
    pub fn first(&self) -> Option<&TickNode> {
        self.nodes.first()
    }

    /// The largest initialised tick.
    #[must_use]
    pub fn last(&self) -> Option<&TickNode> {
        self.nodes.last()
    }

    /// All nodes, in ascending tick order.
    #[must_use]
    pub fn nodes(&self) -> &[TickNode] {
        &self.nodes
    }

    /// Look up a tick exactly.
    #[must_use]
    pub fn get(&self, tick: i32) -> Option<&TickNode> {
        self.by_tick.get(&tick).and_then(|&position| self.nodes.get(position))
    }

    /// The nearest initialised tick to `tick`, preferring the lower one on a tie.
    #[must_use]
    pub fn nearest(&self, tick: i32) -> Option<&TickNode> {
        let upper = self.next_above(tick);
        let lower = self.next_at_or_below(tick);
        match (lower, upper) {
            (Some(low), Some(high)) => {
                if i64::from(high.index()) - i64::from(tick) <
                    i64::from(tick) - i64::from(low.index())
                {
                    Some(high)
                } else {
                    Some(low)
                }
            }
            (Some(node), None) | (None, Some(node)) => Some(node),
            (None, None) => None,
        }
    }

    /// The smallest initialised tick strictly above `tick`.
    #[must_use]
    pub fn next_above(&self, tick: i32) -> Option<&TickNode> {
        self.by_tick
            .range((std::ops::Bound::Excluded(tick), std::ops::Bound::Unbounded))
            .next()
            .and_then(|(_, &position)| self.nodes.get(position))
    }

    /// The largest initialised tick at or below `tick`.
    #[must_use]
    pub fn next_at_or_below(&self, tick: i32) -> Option<&TickNode> {
        self.by_tick
            .range((std::ops::Bound::Unbounded, std::ops::Bound::Included(tick)))
            .next_back()
            .and_then(|(_, &position)| self.nodes.get(position))
    }

    /// The largest initialised tick strictly below `tick`.
    #[must_use]
    pub fn next_below(&self, tick: i32) -> Option<&TickNode> {
        self.by_tick
            .range((std::ops::Bound::Unbounded, std::ops::Bound::Excluded(tick)))
            .next_back()
            .and_then(|(_, &position)| self.nodes.get(position))
    }

    /// Resolve a [`Locate`] request.
    #[must_use]
    pub fn locate(&self, locate: Locate) -> Option<&TickNode> {
        match locate {
            Locate::Exact(tick) => self.get(tick),
            Locate::Nearest(tick) => self.nearest(tick),
        }
    }

    /// The initialised ticks that bracket `tick`, if the pool has them.
    #[must_use]
    pub fn bracketing(&self, tick: i32) -> (Option<&TickNode>, Option<&TickNode>) {
        (self.next_at_or_below(tick), self.next_above(tick))
    }

    /// The active liquidity change when crossing `tick` upwards.
    #[must_use]
    pub fn liquidity_net_at(&self, tick: i32) -> Option<i128> {
        self.get(tick).map(|node| node.tick.liquidity_net.get())
    }
}

impl TickSource for Ticks {
    fn next_boundary_up(&self, tick: i32) -> Option<Boundary> {
        self.next_above(tick).map(|node| Boundary {
            tick: node.index(),
            sqrt_price: node.tick.sqrt_price,
            liquidity_net: node.tick.liquidity_net.get(),
        })
    }

    fn next_boundary_down(&self, tick: i32) -> Option<Boundary> {
        self.next_below(tick).map(|node| Boundary {
            tick: node.index(),
            sqrt_price: node.tick.sqrt_price,
            liquidity_net: node.tick.liquidity_net.get(),
        })
    }
}

/// Reads `0x2::object::UID { id: ID { bytes: address } }` into an [`ObjectID`].
#[derive(Debug, Default, Clone, Copy)]
pub struct UidDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for UidDecoder {
    type Output = ObjectID;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "id" => value = Some(struct_field(driver, IdDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(value, "id")
    }
}

/// Reads `0x2::object::ID { bytes: address }` into an [`ObjectID`].
#[derive(Debug, Default, Clone, Copy)]
pub struct IdDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for IdDecoder {
    type Output = ObjectID;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bytes" => {
                    let address: AccountAddress = birdai_move::address_field(driver)?;
                    value = Some(ObjectID::new(address.into_bytes()));
                }
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(value, "bytes")
    }
}

/// Reads `tick::Tick`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for TickDecoder {
    type Output = Tick;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut index = None;
        let mut sqrt_price = None;
        let mut liquidity_net = None;
        let mut liquidity_gross = None;
        let mut fee_growth_outside_a = None;
        let mut fee_growth_outside_b = None;
        let mut points_growth_outside = None;
        let mut rewards_growth_outside = None;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "index" => index = Some(I32::from_bits(struct_field(driver, I32BitsDecoder)?)),
                "sqrt_price" => sqrt_price = Some(u128_field(driver)?),
                "liquidity_net" => {
                    liquidity_net = Some(I128::from_bits(struct_field(driver, I128BitsDecoder)?));
                }
                "liquidity_gross" => liquidity_gross = Some(u128_field(driver)?),
                "fee_growth_outside_a" => fee_growth_outside_a = Some(u128_field(driver)?),
                "fee_growth_outside_b" => fee_growth_outside_b = Some(u128_field(driver)?),
                "points_growth_outside" => points_growth_outside = Some(u128_field(driver)?),
                "rewards_growth_outside" => {
                    rewards_growth_outside = Some(u128_vec_field(driver)?);
                }
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(Tick {
            index: required(index, "index")?,
            sqrt_price: required(sqrt_price, "sqrt_price")?,
            liquidity_net: required(liquidity_net, "liquidity_net")?,
            liquidity_gross: required(liquidity_gross, "liquidity_gross")?,
            fee_growth_outside_a: required(fee_growth_outside_a, "fee_growth_outside_a")?,
            fee_growth_outside_b: required(fee_growth_outside_b, "fee_growth_outside_b")?,
            points_growth_outside: points_growth_outside.unwrap_or(0),
            rewards_growth_outside: rewards_growth_outside.unwrap_or_default(),
        })
    }
}

/// Reads `0x714a63a0…::i32::I32 { bits: u32 }`.
#[derive(Debug, Default, Clone, Copy)]
struct I32BitsDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for I32BitsDecoder {
    type Output = u32;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut bits = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bits" => bits = Some(birdai_move::u32_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(bits, "bits")
    }
}

/// Reads `0x714a63a0…::i128::I128 { bits: u128 }`.
#[derive(Debug, Default, Clone, Copy)]
struct I128BitsDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for I128BitsDecoder {
    type Output = u128;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut bits = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "bits" => bits = Some(u128_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(bits, "bits")
    }
}

/// Reads a single `skip_list::Node<tick::Tick>`.
///
/// Dynamic fields hold the *value* directly, so the node's own struct is what gets decoded. When a
/// node arrives wrapped in `0x2::dynamic_field::Field<u64, Node<Tick>>` — as it does on the
/// checkpoint path — use [`FieldNodeDecoder`] instead.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickNodeDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for TickNodeDecoder {
    type Output = TickNode;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut score = None;
        let mut nexts = None;
        let mut prev = None;
        let mut tick = None;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "score" => score = Some(u64_field(driver)?),
                "nexts" => {
                    let links: Vec<OptionU64> = struct_vec_field(driver, OptionU64Decoder)?;
                    nexts = Some(links.into_iter().filter_map(OptionU64::to_option).collect());
                }
                "prev" => prev = Some(struct_field(driver, OptionU64Decoder)?.to_option()),
                "value" => tick = Some(struct_field(driver, TickDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(TickNode {
            score: required(score, "score")?,
            nexts: nexts.unwrap_or_default(),
            prev: prev.unwrap_or(None),
            tick: required(tick, "value")?,
        })
    }
}

/// Reads `0x2::dynamic_field::Field<u64, skip_list::Node<Tick>>`, as seen on the checkpoint path.
#[derive(Debug, Default, Clone, Copy)]
pub struct FieldNodeDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for FieldNodeDecoder {
    type Output = TickNode;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut value = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                // The field's `id` is a `UID` and its `name` is the skip-list key; the node itself
                // is `value`.
                "value" => value = Some(struct_field(driver, TickNodeDecoder)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(value, "value")
    }
}

/// Reads `tick::TickManager` far enough to find the tick skip list's metadata.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickManagerDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for TickManagerDecoder {
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

/// Reads `skip_list::SkipList<T>`'s metadata.
#[derive(Debug, Default, Clone, Copy)]
pub struct SkipListDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for SkipListDecoder {
    type Output = SkipListHead;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut node_uid = None;
        let mut head = None;
        let mut tail = None;
        let mut level = 0_u64;
        let mut max_level = 0_u64;
        let mut size = 0_u64;
        let mut seed = 0_u64;

        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "id" => node_uid = Some(struct_field(driver, UidDecoder)?),
                "head" => {
                    let links: Vec<OptionU64> = struct_vec_field(driver, OptionU64Decoder)?;
                    head = links.first().and_then(|link| link.to_option());
                }
                "tail" => tail = Some(struct_field(driver, OptionU64Decoder)?.to_option()),
                "level" => level = u64_field(driver)?,
                "max_level" => max_level = u64_field(driver)?,
                "size" => size = u64_field(driver)?,
                "random" => seed = struct_field(driver, RandomDecoder)?,
                _ => {
                    driver.skip_field()?;
                }
            }
        }

        Ok(SkipListHead {
            node_uid: required(node_uid, "id")?,
            head,
            tail: tail.unwrap_or(None),
            level,
            max_level,
            size,
            seed,
        })
    }
}

/// Reads `random::Random { seed: u64 }`.
#[derive(Debug, Default, Clone, Copy)]
struct RandomDecoder;

impl<'b, 'l> StructDecoder<'b, 'l> for RandomDecoder {
    type Output = u64;

    fn decode(
        &mut self,
        driver: &mut StructDriver<'_, 'b, 'l>,
    ) -> Result<Self::Output, DecodeError> {
        let mut seed = None;
        while let Some(field) = driver.peek_field() {
            match field.name.as_str() {
                "seed" => seed = Some(u64_field(driver)?),
                _ => {
                    driver.skip_field()?;
                }
            }
        }
        required(seed, "seed")
    }
}

#[cfg(test)]
mod tests {
    use birdai_amm::TickSource;
    use birdai_move::{I32, I128};

    use super::{CETUS_TICK_BIAS, Locate, Tick, TickNode, Ticks, score_from_tick, tick_from_score};

    /// Nodes read from mainnet pool A's skip list.
    ///
    /// `(score, tick index, sqrt price, liquidity_net)` — the first two are the check that the
    /// bias is 443_636, and the third is the check that the tick math matches what the pool
    /// stored.
    const MAINNET_NODES: [(u64, i32, u128, i128); 6] = [
        (509_466, 65_830, 495_825_136_860_992_042_578, 1_351_502_914_258),
        (514_686, 71_050, 643_685_510_299_636_945_792, 1_063_380_924),
        // `liquidity_net` is a two's-complement `I128`; this one is negative on chain.
        (514_696, 71_060, 644_007_417_429_774_971_181, -1_950_857_285_114),
        (514_816, 71_180, 647_882_882_935_015_212_980, 212_759_778_363),
        (514_826, 71_190, 648_206_889_171_250_166_865, 39_077_659_554),
        (514_836, 71_200, 648_531_057_443_007_102_076, -6_151_940_512),
    ];

    fn node(score: u64, index: i32, sqrt_price: u128, liquidity_net: i128) -> TickNode {
        TickNode {
            score,
            nexts: vec![],
            prev: None,
            tick: Tick {
                index: I32::new(index),
                sqrt_price,
                liquidity_net: I128::new(liquidity_net),
                liquidity_gross: 0,
                fee_growth_outside_a: 0,
                fee_growth_outside_b: 0,
                points_growth_outside: 0,
                rewards_growth_outside: vec![],
            },
        }
    }

    fn mainnet_index() -> Result<Ticks, super::TickError> {
        let nodes =
            MAINNET_NODES.iter().map(|&(score, index, price, net)| node(score, index, price, net));
        Ticks::new(None, None, nodes)
    }

    #[test]
    fn score_bias_round_trips_and_matches_mainnet() {
        for &(score, index, _, _) in &MAINNET_NODES {
            assert_eq!(score_from_tick(index), score);
            assert_eq!(tick_from_score(score), index);
        }
        assert_eq!(score_from_tick(-CETUS_TICK_BIAS), 0);
        assert_eq!(tick_from_score(0), -CETUS_TICK_BIAS);
    }

    #[test]
    fn accepts_mainnet_nodes_and_matches_the_tick_math() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        assert_eq!(index.len(), MAINNET_NODES.len());
        Ok(())
    }

    #[test]
    fn rejects_a_node_whose_stored_price_disagrees_with_the_tick_math() {
        // Far outside the relative tolerance: this node is not the tick it claims to be.
        let bad = node(514_696, 71_060, 644_007_417_429_774_971_181 + 1_000_000_000, 0);
        let outcome = Ticks::new(None, None, [bad]);
        assert!(matches!(outcome, Err(super::TickError::PriceMismatch { .. })));
    }

    /// Mainnet keeps a minority of ticks one ulp below the tick math; that must be accepted and
    /// counted, not rejected.
    #[test]
    fn accepts_and_counts_a_one_ulp_deviation() -> Result<(), super::TickError> {
        let exact = node(514_696, 71_060, 644_007_417_429_774_971_181, 0);
        let low = node(514_686, 71_050, 643_685_510_299_636_945_792 - 1, 0);
        let index = Ticks::new(None, None, [exact, low])?;
        assert_eq!(index.max_price_deviation(), 1);
        assert_eq!(index.exact_price_nodes(), 1);
        assert_eq!(index.price_deviations().get(&-1), Some(&1));
        Ok(())
    }

    #[test]
    fn rejects_a_size_that_disagrees_with_the_node_count() {
        let outcome =
            Ticks::new(None, Some(999), MAINNET_NODES.iter().map(|&(s, i, p, n)| node(s, i, p, n)));
        assert!(matches!(
            outcome,
            Err(super::TickError::SizeMismatch { expected: 999, actual: 6 })
        ));
    }

    #[test]
    fn rejects_a_duplicated_node() {
        let a = node(514_696, 71_060, 644_007_417_429_774_971_181, 0);
        let outcome = Ticks::new(None, None, [a.clone(), a]);
        assert!(matches!(outcome, Err(super::TickError::DuplicateNode(_))));
    }

    #[test]
    fn rejects_a_dangling_forward_link() {
        let mut first = node(514_686, 71_050, 643_685_510_299_636_945_792, 0);
        first.nexts = vec![123_456_789];
        let outcome = Ticks::new(None, None, [first]);
        assert!(matches!(outcome, Err(super::TickError::DanglingLink { .. })));
    }

    #[test]
    fn rejects_a_link_pointing_at_the_node_itself() {
        // A self-link resolves in the score map, so only the `!= position` half of the check
        // catches it. Without that half a one-node cycle would validate as a healthy list.
        let mut only = node(514_686, 71_050, 643_685_510_299_636_945_792, 0);
        only.nexts = vec![514_686];
        let outcome = Ticks::new(None, None, [only]);
        assert!(matches!(
            outcome,
            Err(super::TickError::DanglingLink { score: 514_686, neighbour: 514_686 })
        ));
    }

    #[test]
    fn a_larger_negative_deviation_lands_in_its_own_bucket() -> Result<(), super::TickError> {
        // A deviation of -1 and a deviation of -1000 must not histogram together: the magnitude
        // comes from a subtraction, and dividing the prices instead would collapse every small
        // negative deviation to -1.
        let computed = birdai_amm::sqrt_price_at_tick(71_050)?;
        let low = node(514_686, 71_050, computed - 1_000, 0);
        let index = Ticks::new(None, None, [low])?;
        assert_eq!(index.price_deviations().get(&-1_000), Some(&1));
        assert_eq!(index.price_deviations().get(&-1), None);
        assert_eq!(index.max_price_deviation(), 1_000);
        Ok(())
    }

    #[test]
    fn brackets_the_current_tick_the_way_mainnet_does() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        // The tick transaction T ran at, which is not on the pool's spacing grid.
        let current = 71_162;
        let (below, above) = index.bracketing(current);
        assert_eq!(below.map(TickNode::index), Some(71_060));
        assert_eq!(above.map(TickNode::index), Some(71_180));

        // The next boundary above is what the swap must not reach.
        let boundary =
            index.next_boundary_up(current).ok_or(super::TickError::TickOutOfRange(current))?;
        assert_eq!(boundary.tick, 71_180);
        assert_eq!(boundary.sqrt_price, 647_882_882_935_015_212_980);
        assert_eq!(boundary.liquidity_net, 212_759_778_363);
        Ok(())
    }

    #[test]
    fn nearest_prefers_the_lower_tick_on_a_tie() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        // The gap between 71_060 and 71_180 is 120, so the midpoint is 71_120.
        assert_eq!(index.nearest(71_065).map(TickNode::index), Some(71_060));
        assert_eq!(index.nearest(71_119).map(TickNode::index), Some(71_060));
        assert_eq!(index.nearest(71_120).map(TickNode::index), Some(71_060));
        assert_eq!(index.nearest(71_121).map(TickNode::index), Some(71180));
        assert_eq!(index.nearest(0).map(TickNode::index), Some(65_830));
        assert_eq!(index.nearest(999_999).map(TickNode::index), Some(71_200));
        Ok(())
    }

    #[test]
    fn locate_resolves_exact_and_nearest() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        assert_eq!(index.locate(Locate::Exact(71_180)).map(TickNode::index), Some(71_180));
        assert!(index.locate(Locate::Exact(71_162)).is_none());
        assert_eq!(index.locate(Locate::Nearest(71_162)).map(TickNode::index), Some(71_180));
        Ok(())
    }

    #[test]
    fn next_below_is_strict() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        assert_eq!(index.next_below(71_180).map(TickNode::index), Some(71_060));
        assert_eq!(index.next_above(71_180).map(TickNode::index), Some(71_190));
        Ok(())
    }

    #[test]
    fn an_empty_index_answers_nothing_rather_than_guessing() {
        let index = Ticks::default();
        assert!(index.is_empty());
        assert!(index.nearest(0).is_none());
        assert!(index.next_above(0).is_none());
        assert!(index.next_boundary_up(0).is_none());
    }

    #[test]
    fn liquidity_net_is_signed() -> Result<(), super::TickError> {
        let index = mainnet_index()?;
        assert_eq!(index.liquidity_net_at(71_180), Some(212_759_778_363));
        assert_eq!(index.liquidity_net_at(71_180), Some(212_759_778_363));
        // A negative net, as seen when a range's upper tick is crossed upwards.
        let negative = node(1, 100, birdai_amm::sqrt_price_at_tick(100)?, -5);
        let index = Ticks::new(None, None, [negative])?;
        assert_eq!(index.liquidity_net_at(100), Some(-5));
        Ok(())
    }

    #[test]
    fn accessors_describe_a_built_index() -> Result<(), super::TickError> {
        use sui_types::base_types::ObjectID;

        let uid = ObjectID::new([0x7f; 32]);
        let nodes = MAINNET_NODES.iter().map(|&(s, i, p, n)| node(s, i, p, n));
        let index = Ticks::new(Some(uid), Some(6), nodes)?;
        assert!(!index.is_empty(), "six nodes is not empty");
        assert_eq!(index.len(), 6);
        assert_eq!(index.node_uid(), Some(uid));
        assert_eq!(index.first().map(TickNode::index), Some(65_830));
        assert_eq!(index.last().map(TickNode::index), Some(71_200));
        assert_eq!(index.nodes().len(), 6);
        assert!(
            index.nodes().windows(2).all(|pair| pair[0].index() < pair[1].index()),
            "nodes must come out in ascending tick order"
        );
        Ok(())
    }

    #[test]
    fn next_boundary_down_mirrors_the_tick_below() -> Result<(), super::TickError> {
        use birdai_amm::TickSource;

        let index = mainnet_index()?;
        let boundary =
            index.next_boundary_down(71_162).ok_or(super::TickError::TickOutOfRange(71_162))?;
        assert_eq!(boundary.tick, 71_060);
        assert_eq!(boundary.sqrt_price, 644_007_417_429_774_971_181);
        assert_eq!(boundary.liquidity_net, -1_950_857_285_114);
        assert!(index.next_boundary_down(65_830).is_none());
        Ok(())
    }

    #[test]
    fn a_deviation_exactly_at_tolerance_is_accepted() -> Result<(), super::TickError> {
        // `deviation > tolerance` rejects; `>=` would reject this node. The tolerance at this
        // price is `computed >> 48`, several thousand units, so an exact-boundary deviation is
        // constructible without touching the grid price itself.
        let computed = birdai_amm::sqrt_price_at_tick(71_060)?;
        let tolerance = birdai_amm::tick_price_tolerance(computed);
        assert!(tolerance > 1, "this price must have a non-trivial tolerance");
        let edge = node(514_696, 71_060, computed + tolerance, 0);
        let index = Ticks::new(None, None, [edge])?;
        assert_eq!(index.max_price_deviation(), tolerance);
        let over = node(514_696, 71_060, computed + tolerance + 1, 0);
        assert!(matches!(
            Ticks::new(None, None, [over]),
            Err(super::TickError::PriceMismatch { .. })
        ));
        Ok(())
    }
}
