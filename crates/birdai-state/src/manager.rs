//! Checkpoint-driven in-memory venue state.
//!
//! # The boundary
//!
//! Exactly three things cross from the checkpoint into typed state:
//!
//! ```text
//! raw object (BCS bytes + StructTag + SequenceNumber + Owner)
//!    → LayoutSource::layout(tag)          [layout, cached, upgrade-aware]
//!    → Venue::decode(bytes, layout)       [one pass, name-matched, no tree]
//!    → Arc<VenueSlot>                     [published per checkpoint]
//! ```
//!
//! Nothing above the boundary knows about gRPC, checkpoints, or BCS. That is what makes the
//! "objects from inside a validator instead of from the stream" question a source swap rather than
//! a rewrite: [`crate`] takes objects, and where they came from is somebody else's problem.
//!
//! # Publication
//!
//! Slots live in `scc::HashMap`, each holding an immutable [`VenueSlot`]. Publication is
//! per-object: each venue slot is replaced as its object decodes, and the commit counter is
//! bumped after the whole checkpoint is applied. A reader that arrives mid-checkpoint can
//! therefore see a mix of old and new slots — what the counter gives is a monotone "checkpoints
//! applied" watermark, not a snapshot barrier. Pricing code must treat a single slot as
//! consistent (it always is: slots are immutable) without assuming two slots are from the same
//! checkpoint.
//!
//! # The three hard problems
//!
//! * **New pools** — identity is by `module::name` (see [`birdai_venue::venue_kind_of`]), not by an
//!   allow-list, so a pool deployed under a new package is picked up on first sight. The name is
//!   only a hint: the resolved layout's field set is the test, and collisions are counted as
//!   `unrecognised` rather than `failed`.
//! * **Dynamic-field churn** — tick nodes are dynamic fields of an *inner UID*, so they are routed
//!   by `derive_dynamic_field_id`-shaped ownership rather than by the containing object. Children
//!   are indexed only for the inner UIDs of tracked Cetus pools — never for unrelated parents, so a
//!   ledger with ~999k user entries cannot bloat the index — and each parent's set is bounded, with
//!   drops counted and warned rather than silent.
//! * **Package upgrades** — see [`StateManager::package_observations`]: a package that appears in a
//!   checkpoint's object set is recorded under **its original package id**, because an upgraded
//!   package has a new object id while the layouts that mention it are canonicalised to the
//!   original. Getting this wrong is the difference between invalidating on upgrade and never
//!   noticing one.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use birdai_resolve::{Fingerprint, LayoutSource};
use birdai_tick::{SkipListHead, Ticks};
use birdai_venue::{AnyVenue, VenueKind, decode_venue, venue_kind_of};
use move_core_types::{account_address::AccountAddress, language_storage::StructTag};
use rayon::prelude::*;
use scc::HashMap as SccHashMap;
use sui_types::{
    base_types::ObjectID, effects::TransactionEffectsAPI, full_checkpoint_content::Checkpoint,
    object::Object,
};

use crate::error::StateError;

/// One object's decode result: its identity, the layout it was decoded with, and the typed venue.
type DecodeOutcome = Result<(ObjectID, u64, StructTag, Fingerprint, AnyVenue), StateError>;

/// One venue at one version.
#[derive(Debug, Clone)]
pub struct VenueSlot {
    /// The object id.
    pub id: ObjectID,
    /// The version this state was read from.
    pub version: u64,
    /// The venue's type.
    pub tag: StructTag,
    /// Which kind of venue it is.
    pub kind: VenueKind,
    /// The typed state.
    pub venue: AnyVenue,
    /// Fingerprint of the layout it was decoded with.
    ///
    /// Recorded so tooling can assert a replay decoded with the same schema that was live at the
    /// time; the manager itself warns when a tracked venue's fingerprint changes under it (a
    /// schema change without a matching package observation) and publishes the new state.
    pub layout: Fingerprint,
    /// The tick index, for pools whose nodes have been loaded.
    pub ticks: Option<Arc<Ticks>>,
}

impl VenueSlot {
    /// The price state, if the venue has one.
    #[must_use]
    pub fn price_state(&self) -> Option<birdai_venue::PriceState> {
        self.venue.price_state()
    }
}

/// What a checkpoint did to the state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// The checkpoint that was applied.
    pub checkpoint: u64,
    /// Objects examined in the change set.
    pub objects_examined: usize,
    /// Venues whose state was replaced.
    pub venues_updated: usize,
    /// Venues seen for the first time.
    pub venues_created: usize,
    /// Venues removed from the tracked set.
    pub venues_removed: usize,
    /// Dynamic-field children indexed.
    pub children_indexed: usize,
    /// Dynamic-field children dropped because a parent's set was already at capacity.
    pub children_dropped: usize,
    /// Package observations applied.
    pub packages_observed: usize,
    /// Cached layouts evicted because a package moved.
    pub layouts_invalidated: u64,
    /// Objects that were present but could not be typed.
    pub failures: usize,
    /// Objects whose `module::name` looked like a venue but whose layout is a different struct.
    ///
    /// Not a failure: several mainnet packages define a `pool::Pool`, so a name collision is
    /// expected and is exactly what the layout-shape check exists to catch.
    pub unrecognised: usize,
}

/// Aggregate view of what the manager holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManagerStats {
    /// Tracked venues.
    pub venues: usize,
    /// Tracked dynamic-field children across all parents.
    pub children: usize,
    /// Venues by kind.
    pub by_kind: BTreeMap<&'static str, usize>,
    /// Checkpoints applied.
    pub checkpoints: u64,
}

/// Tracks dynamic-field children of the parents we price.
///
/// Tick nodes hang off the skip list's **inner UID**, so a child's owner is not the containing
/// object. The index therefore keys on the UID, and is bounded so that a parent with millions of
/// unrelated children (Navi's `user_info` table has ~999k) cannot exhaust memory.
#[derive(Debug)]
struct ChildSet {
    /// Child field ids seen for this parent.
    field_ids: HashSet<ObjectID>,
    /// Cap on how many children to retain.
    capacity: usize,
}

impl ChildSet {
    fn new(capacity: usize) -> Self {
        Self { field_ids: HashSet::new(), capacity }
    }

    fn insert(&mut self, field_id: ObjectID) -> bool {
        if self.field_ids.len() >= self.capacity && !self.field_ids.contains(&field_id) {
            return false;
        }
        self.field_ids.insert(field_id)
    }
}

/// Keeps typed venue state current from checkpoints.
#[derive(Debug)]
pub struct StateManager {
    slots: SccHashMap<ObjectID, Arc<VenueSlot>>,
    children: SccHashMap<ObjectID, ChildSet>,
    venues_created: AtomicU64,
    venues_updated: AtomicU64,
    venues_removed: AtomicU64,
    children_indexed: AtomicU64,
    children_dropped: AtomicU64,
    package_observations: AtomicU64,
    failures: AtomicU64,
    unrecognised: AtomicU64,
    checkpoints: AtomicU64,
    last_applied: AtomicU64,
    child_capacity: usize,
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StateManager {
    /// Default cap on children retained per parent.
    ///
    /// Cetus pools have a few hundred ticks, which is far below this; the cap exists for parents
    /// such as Navi's `user_info` table, whose contents we have no reason to price.
    pub const DEFAULT_CHILD_CAPACITY: usize = 4096;

    /// A manager with the default child cap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_child_capacity(Self::DEFAULT_CHILD_CAPACITY)
    }

    /// A manager with an explicit child cap.
    #[must_use]
    pub fn with_child_capacity(child_capacity: usize) -> Self {
        Self {
            slots: SccHashMap::new(),
            children: SccHashMap::new(),
            venues_created: AtomicU64::new(0),
            venues_updated: AtomicU64::new(0),
            venues_removed: AtomicU64::new(0),
            children_indexed: AtomicU64::new(0),
            children_dropped: AtomicU64::new(0),
            package_observations: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            unrecognised: AtomicU64::new(0),
            checkpoints: AtomicU64::new(0),
            last_applied: AtomicU64::new(0),
            child_capacity,
        }
    }

    /// The venue currently held for `id`, if any.
    #[must_use]
    pub fn venue(&self, id: &ObjectID) -> Option<Arc<VenueSlot>> {
        self.slots.read_sync(id, |_key, value| value.clone())
    }

    /// Every tracked venue.
    #[must_use]
    pub fn venues(&self) -> Vec<Arc<VenueSlot>> {
        let mut out = Vec::new();
        self.slots.iter_sync(|_key, value| {
            out.push(value.clone());
            true
        });
        out.sort_by_key(|slot| slot.id);
        out
    }

    /// The tick index held for a pool, if its children have been loaded.
    #[must_use]
    pub fn ticks(&self, id: &ObjectID) -> Option<Arc<Ticks>> {
        self.venue(id).and_then(|slot| slot.ticks.clone())
    }

    /// Child field ids recorded for a parent UID.
    #[must_use]
    pub fn child_fields(&self, parent: &ObjectID) -> Vec<ObjectID> {
        self.children
            .read_sync(parent, |_key, set| set.field_ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Attach a tick index to an already-tracked pool.
    ///
    /// The nodes arrive separately from the pool object — they are dynamic fields of an inner UID
    /// — so they are loaded deliberately rather than as part of the checkpoint walk. Unlike
    /// [`StateManager::install_ticks`], this does not assert the skip list's declared size:
    /// children can only be listed as of the present, so an index loaded for any pool that has
    /// traded since will legitimately disagree with historical metadata.
    pub fn set_ticks(&self, id: ObjectID, ticks: Arc<Ticks>) {
        let _ = self.slots.update_sync(&id, |_key, slot| {
            let previous = slot.ticks.clone();
            *slot = Arc::new(VenueSlot { ticks: Some(ticks), ..(**slot).clone() });
            previous
        });
    }

    fn venues_updated_now(&self) {
        self.venues_updated.fetch_add(1, Ordering::Relaxed);
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> ManagerStats {
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        self.slots.iter_sync(|_key, value| {
            *by_kind.entry(value.kind.label()).or_default() += 1;
            true
        });
        let mut children = 0_usize;
        self.children.iter_sync(|_key, set| {
            children += set.field_ids.len();
            true
        });
        ManagerStats {
            venues: self.slots.len(),
            children,
            by_kind,
            checkpoints: self.checkpoints.load(Ordering::Relaxed),
        }
    }

    /// Apply one checkpoint.
    ///
    /// Order of operations matters:
    ///
    /// 1. package observations first, so any layout the checkpoint invalidated is re-resolved
    ///    before it is used to type the objects in the same checkpoint;
    /// 2. distinct venue tags resolved once, not once per object;
    /// 3. decoding on the blocking pool with `rayon`, because the layouts are already in hand and
    ///    an `async` worker must not sit on CPU-bound work;
    /// 4. slots published per object, with the commit counter bumped at the end.
    ///
    /// Versions are monotone per object: an incoming version older than the slot's is an
    /// [`StateError::OutOfOrder`] (applied twice, or out of order — either would silently serve
    /// a stale price), while re-applying the version already held is a no-op so a retried
    /// checkpoint stays idempotent. Checkpoints themselves are monotone too: a checkpoint at or
    /// below the last applied sequence is a duplicate delivery and is skipped whole, which is
    /// what makes re-applying a checkpoint that touched one object several times safe.
    pub async fn apply_checkpoint<L: LayoutSource + ?Sized>(
        &self,
        layouts: &L,
        checkpoint: &Checkpoint,
    ) -> Result<UpdateReport, StateError> {
        let sequence = checkpoint.summary.sequence_number;
        let mut report = UpdateReport { checkpoint: sequence, ..UpdateReport::default() };

        // Duplicate delivery: an ordered stream only replays a checkpoint on retry, and
        // skipping is the safe direction — serving it again could only move a slot backwards
        // through an intermediate version the stream already superseded.
        let last = self.last_applied.load(Ordering::Relaxed);
        if last != 0 && sequence <= last {
            tracing::debug!(
                checkpoint = sequence,
                last_applied = last,
                "skipping an already-applied checkpoint"
            );
            return Ok(report);
        }

        // (1) Package upgrades.
        let observations = self.package_observations(checkpoint);
        report.packages_observed = observations.len();
        if !observations.is_empty() {
            layouts.note_packages(observations);
            self.package_observations.fetch_add(report.packages_observed as u64, Ordering::Relaxed);
        }

        // (2) Collect the changed objects that could be venues, and the children.
        let mut candidates: Vec<(&Object, StructTag)> = Vec::new();
        let mut removed: Vec<ObjectID> = Vec::new();
        let mut child_parents: Vec<(ObjectID, ObjectID)> = Vec::new();

        for transaction in &checkpoint.transactions {
            for change in transaction.effects.object_changes() {
                report.objects_examined += 1;
                let Some(output_version) = change.output_version else {
                    removed.push(change.id);
                    continue;
                };
                let key = sui_types::storage::ObjectKey(change.id, output_version);
                let Some(object) = checkpoint.object_set.get(&key) else {
                    // The effects name an object the set does not carry. Filtered captures
                    // (see `fetch`) legitimately produce this; an unfiltered stream should
                    // not, so it is counted as a failure rather than swallowed.
                    let missing = StateError::MissingObject {
                        id: change.id.to_canonical_string(true),
                        version: output_version.value(),
                    };
                    tracing::warn!(%missing, "change set names an object outside the object set");
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    report.failures += 1;
                    continue;
                };
                let Some(tag) = object.struct_tag() else {
                    continue;
                };
                if venue_kind_of(&tag).is_some() {
                    candidates.push((object, tag));
                } else if is_dynamic_field(&tag) &&
                    let Some(parent) = birdai_resolve::object::dynamic_field_parent(object)
                {
                    child_parents.push((parent, change.id));
                }
            }
        }

        // (3) Resolve each distinct tag once. A tag that does not resolve — a package the
        // source has pruned, or never served — skips its objects without killing the
        // checkpoint: a follower that died on one unresolvable object would never stay
        // current, and the skip is counted rather than silent.
        let mut distinct: Vec<StructTag> = Vec::new();
        let mut seen: HashSet<StructTag> = HashSet::new();
        for (_, tag) in &candidates {
            if seen.insert(tag.clone()) {
                distinct.push(tag.clone());
            }
        }
        let mut resolved: HashMap<
            StructTag,
            (Arc<move_core_types::annotated_value::MoveTypeLayout>, Fingerprint),
        > = HashMap::with_capacity(distinct.len());
        for tag in &distinct {
            match layouts.layout(tag).await {
                Ok(layout) => {
                    let fingerprint = birdai_resolve::layout::fingerprint(&layout);
                    resolved.insert(tag.clone(), (layout, fingerprint));
                }
                Err(error) => {
                    tracing::warn!(
                        tag = %tag.to_canonical_string(true),
                        %error,
                        "could not resolve a layout; skipping its objects for this checkpoint"
                    );
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    report.failures += 1;
                }
            }
        }
        candidates.retain(|(_, tag)| resolved.contains_key(tag));

        // (4) Decode on the blocking pool: no I/O is left, and every layout is in hand. The
        // BCS is copied out of the checkpoint first so the spawned work owns its inputs — a
        // venue object is under a kilobyte, so the copy is cheaper than holding the
        // checkpoint across the thread hop, and the `async` worker stays free for I/O.
        let mut untyped: Vec<(ObjectID, u64, StructTag, Fingerprint, Vec<u8>)> =
            Vec::with_capacity(candidates.len());
        for (object, tag) in candidates {
            let (_, fingerprint) = resolved
                .get(&tag)
                .ok_or_else(|| StateError::MissingLayout(tag.to_canonical_string(true)))?;
            let Some(move_object) = object.data.try_as_move() else {
                return Err(StateError::NotMoveObject(object.id().to_canonical_string(true)));
            };
            untyped.push((
                object.id(),
                object.version().value(),
                tag,
                *fingerprint,
                move_object.contents().to_vec(),
            ));
        }
        let decoded: Vec<DecodeOutcome> = tokio::task::spawn_blocking(move || {
            untyped
                .into_par_iter()
                .map(|(id, version, tag, fingerprint, contents)| {
                    let layout = resolved
                        .get(&tag)
                        .ok_or_else(|| StateError::MissingLayout(tag.to_canonical_string(true)))?;
                    let venue = decode_venue(&contents, &tag, &layout.0)?;
                    Ok((id, version, tag, fingerprint, venue))
                })
                .collect()
        })
        .await?;

        // (5) Publish, one slot at a time; the commit counter moves last.
        for outcome in decoded {
            match outcome {
                Ok((id, version, tag, layout, venue)) => {
                    // One lookup serves the monotonicity check, the tick carry-over and the
                    // schema-change watch below.
                    let existing = self.slots.read_sync(&id, |_key, slot| {
                        (slot.version, slot.ticks.clone(), slot.layout)
                    });
                    // Monotone per object: older than the slot is an out-of-order apply,
                    // equal is a retried checkpoint and stays a no-op.
                    if !should_publish(&id, existing.as_ref().map(|(held, _, _)| *held), version)? {
                        continue;
                    }
                    let kind = venue.kind();
                    // Tick nodes are loaded deliberately rather than walked, so carry them over.
                    let ticks = existing.as_ref().and_then(|(_, ticks, _)| ticks.clone());
                    if let Some((_, _, previous)) = existing.as_ref() &&
                        *previous != layout
                    {
                        tracing::warn!(
                            object = %id.to_canonical_string(true),
                            "tracked venue decoded under a changed layout; publishing the new schema"
                        );
                    }
                    let previous = self.slots.upsert_sync(
                        id,
                        Arc::new(VenueSlot { id, version, tag, kind, venue, layout, ticks }),
                    );
                    report.venues_updated += 1;
                    if previous.is_none() {
                        self.venues_created.fetch_add(1, Ordering::Relaxed);
                        report.venues_created += 1;
                    } else {
                        self.venues_updated_now();
                    }
                }
                // A name collision — another package's `pool::Pool` — is expected and is not a
                // failure. Everything else is, because it means a venue we thought we could type
                // did not decode.
                Err(StateError::Venue(birdai_venue::VenueError::UnknownVenue(reason))) => {
                    self.unrecognised.fetch_add(1, Ordering::Relaxed);
                    report.unrecognised += 1;
                    tracing::debug!(reason, "skipped an object that only looks like a venue");
                }
                Err(error) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    report.failures += 1;
                    tracing::warn!(%error, "could not type an object in this checkpoint");
                }
            }
        }

        for id in removed {
            if self.slots.remove_sync(&id).is_some() {
                self.venues_removed.fetch_add(1, Ordering::Relaxed);
                report.venues_removed += 1;
            }
            // A deleted parent's child entries die with it; otherwise a churned pool would
            // leave its tick ids behind forever.
            self.children.remove_sync(&id);
        }

        // Children are indexed only for the inner UIDs of tracked Cetus pools. Anything else —
        // a lending ledger's user table, a staking pool's vaults — is state we never price, and
        // indexing it would let one busy parent bloat the map without bound.
        let mut priced: HashSet<ObjectID> = HashSet::new();
        self.slots.iter_sync(|_key, slot| {
            if let AnyVenue::Cetus(pool) = &slot.venue {
                priced.insert(pool.ticks.node_uid);
            }
            true
        });
        let grouped = group_children(&child_parents);
        for (parent, field_ids) in &grouped {
            if !priced.contains(parent) {
                continue;
            }
            let mut occupied = self
                .children
                .entry_sync(*parent)
                .or_insert_with(|| ChildSet::new(self.child_capacity));
            let set = occupied.get_mut();
            let mut inserted = 0_usize;
            let mut dropped = 0_usize;
            for field_id in field_ids {
                if set.insert(*field_id) {
                    inserted += 1;
                } else if !set.field_ids.contains(field_id) {
                    dropped += 1;
                }
            }
            if inserted > 0 {
                self.children_indexed.fetch_add(inserted as u64, Ordering::Relaxed);
                report.children_indexed += inserted;
            }
            if dropped > 0 {
                tracing::warn!(
                    parent = %parent.to_canonical_string(true),
                    dropped,
                    "child set at capacity; dropped dynamic-field children"
                );
                self.children_dropped.fetch_add(dropped as u64, Ordering::Relaxed);
                report.children_dropped += dropped;
            }
        }

        self.checkpoints.fetch_add(1, Ordering::Relaxed);
        self.last_applied.store(sequence, Ordering::Relaxed);
        Ok(report)
    }

    /// Package versions observed in a checkpoint, keyed by the package a layout would reference.
    ///
    /// A Sui package upgrade produces a **new package object with a new id** whose
    /// `type_origin_table` still points at the original for inherited types, and the layout
    /// resolver canonicalises struct tags to exactly those original ids. Recording the observation
    /// under the *original* id is therefore what makes an upgrade invalidate the layouts that
    /// mention it; recording it under the new id would invalidate a layout nobody has cached.
    #[must_use]
    pub fn package_observations(&self, checkpoint: &Checkpoint) -> Vec<(AccountAddress, u64)> {
        let mut out = Vec::new();
        for object in checkpoint.object_set.iter() {
            let Some(package) = object.data.try_as_package() else {
                continue;
            };
            let version = object.version().value();
            out.push((AccountAddress::from(package.original_package_id()), version));
            out.push((AccountAddress::from(package.id()), version));
        }
        out
    }

    /// Replace a pool's tick index from freshly decoded nodes.
    ///
    /// The skip list's declared `size` is asserted against the number of nodes decoded: a partial
    /// page walk is the failure mode that silently misprices every quote, so it is a hard error.
    /// The nodes are also checked against the tracked pool's spacing grid when the pool is known.
    pub fn install_ticks(
        &self,
        id: ObjectID,
        head: &SkipListHead,
        nodes: impl IntoIterator<Item = birdai_tick::TickNode>,
    ) -> Result<Arc<Ticks>, StateError> {
        let ticks = Ticks::new(Some(head.node_uid), Some(head.size), nodes)?;
        if let Some(slot) = self.venue(&id) &&
            let AnyVenue::Cetus(pool) = &slot.venue
        {
            ticks.validate_spacing(pool.tick_spacing)?;
        }
        let ticks = Arc::new(ticks);
        self.set_ticks(id, ticks.clone());
        Ok(ticks)
    }
}

/// True when a tag is a dynamic field (`0x2::dynamic_field::Field<K, V>`) or a dynamic object field
/// (`0x2::dynamic_object_field::Wrapper<V>`).
///
/// Both are children of the UID they were added to, which for a container inlined in another
/// object is an inner UID rather than the containing object.
#[must_use]
fn is_dynamic_field(tag: &StructTag) -> bool {
    birdai_move::is_tag(tag, birdai_move::SUI_FRAMEWORK, "dynamic_field", "Field") ||
        birdai_move::is_tag(tag, birdai_move::SUI_FRAMEWORK, "dynamic_object_field", "Wrapper")
}

/// Group child field ids by the parent UID they belong to.
#[must_use]
pub fn group_children(children: &[(ObjectID, ObjectID)]) -> BTreeMap<ObjectID, Vec<ObjectID>> {
    let mut out: BTreeMap<ObjectID, Vec<ObjectID>> = BTreeMap::new();
    for (parent, child) in children {
        out.entry(*parent).or_default().push(*child);
    }
    out
}

/// Whether `incoming` may replace the slot, given the version already held.
///
/// * no slot yet, or a newer version — publish;
/// * the same version — already applied, skip so a retried checkpoint is idempotent;
/// * an older version — out-of-order, which would silently serve a stale price.
fn should_publish(id: &ObjectID, current: Option<u64>, incoming: u64) -> Result<bool, StateError> {
    match current {
        None => Ok(true),
        Some(current) if incoming > current => Ok(true),
        Some(current) if incoming == current => Ok(false),
        Some(current) => {
            Err(StateError::OutOfOrder { id: id.to_canonical_string(true), current, incoming })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use birdai_resolve::fixture::{FixtureLayoutSource, Fixtures};
    use sui_types::base_types::ObjectID;

    use super::{StateManager, group_children, should_publish};
    use crate::error::StateError;

    /// Object A: the Cetus pool transaction T traded.
    const POOL_A: &str = "0x51e883ba7c0b566a26cbc8a94cd33eb0abd418a77cc1e60ad22fd9b1f29cd2ab";

    /// Checkpoint carrying transaction T, as captured in `fixtures/`.
    const TX_CHECKPOINT: u64 = 320_577_815;

    /// The pool version transaction T consumed and the one it produced.
    const PRE_VERSION: u64 = 995_150_484;
    const POST_VERSION: u64 = 995_150_494;

    fn fixtures() -> Arc<Fixtures> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        Arc::new(Fixtures::load(&dir).unwrap_or_else(|error| unreachable!("{error:?}")))
    }

    fn pool_a() -> ObjectID {
        POOL_A.parse().unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn versions_are_monotone_per_object() {
        let id = pool_a();
        let publish = |current, incoming| {
            should_publish(&id, current, incoming).unwrap_or_else(|error| unreachable!("{error:?}"))
        };
        assert!(publish(None, POST_VERSION), "first sight publishes");
        assert!(publish(Some(PRE_VERSION), POST_VERSION), "newer publishes");
        assert!(!publish(Some(POST_VERSION), POST_VERSION), "same version skips");
        assert!(matches!(
            should_publish(&id, Some(POST_VERSION), PRE_VERSION),
            Err(StateError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn child_fields_group_by_parent() {
        use move_core_types::account_address::AccountAddress;

        let parent = ObjectID::from(AccountAddress::new([1; 32]));
        let other = ObjectID::from(AccountAddress::new([2; 32]));
        let field = ObjectID::from(AccountAddress::new([3; 32]));
        let grouped = group_children(&[(parent, field), (other, field)]);
        assert_eq!(grouped[&parent], vec![field]);
        assert_eq!(grouped[&other], vec![field]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn applies_the_captured_checkpoint_offline() {
        let fixtures = fixtures();
        let layouts = FixtureLayoutSource::new(fixtures.clone());
        let checkpoint =
            fixtures.checkpoint(TX_CHECKPOINT).unwrap_or_else(|error| unreachable!("{error:?}"));

        let manager = StateManager::new();
        let report = manager
            .apply_checkpoint(&layouts, &checkpoint)
            .await
            .unwrap_or_else(|error| unreachable!("{error:?}"));
        assert!(report.venues_updated > 0, "the checkpoint must touch a venue: {report:?}");
        assert_eq!(report.failures, 0, "every touched object must resolve offline: {report:?}");

        // Pool A is tracked at the version transaction T produced, which is newer than the
        // pre-state the reproduction quotes from.
        let slot = manager.venue(&pool_a()).unwrap_or_else(|| unreachable!("pool A is tracked"));
        assert_eq!(slot.version, POST_VERSION);
        assert!(slot.version > PRE_VERSION);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reapplying_a_checkpoint_is_idempotent() {
        let fixtures = fixtures();
        let layouts = FixtureLayoutSource::new(fixtures.clone());
        let checkpoint =
            fixtures.checkpoint(TX_CHECKPOINT).unwrap_or_else(|error| unreachable!("{error:?}"));

        let manager = StateManager::new();
        manager
            .apply_checkpoint(&layouts, &checkpoint)
            .await
            .unwrap_or_else(|error| unreachable!("{error:?}"));
        let again = manager
            .apply_checkpoint(&layouts, &checkpoint)
            .await
            .unwrap_or_else(|error| unreachable!("{error:?}"));
        assert_eq!(again.venues_updated, 0, "nothing new on re-apply: {again:?}");
        assert_eq!(
            manager.venue(&pool_a()).unwrap_or_else(|| unreachable!("pool A is tracked")).version,
            POST_VERSION
        );
    }
}
