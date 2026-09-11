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
//! Slots live in `scc::HashMap`, each holding an immutable [`VenueSlot`]. A reader either sees the
//! whole checkpoint or none of it: every slot is replaced before any reader is told the checkpoint
//! finished, and the commit counter is bumped last.
//!
//! # The three hard problems
//!
//! * **New pools** — identity is by `module::name` (see [`birdai_venue::venue_kind_of`]), not by an
//!   allow-list, so a pool deployed under a new package is picked up on first sight.
//! * **Dynamic-field churn** — tick nodes are dynamic fields of an *inner UID*, so they are routed
//!   by `derive_dynamic_field_id`-shaped ownership rather than by the containing object. Children
//!   are indexed only for parents we actually price, and the skip list's declared `size` is used as
//!   an assertion against the number of children found.
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
    /// Fingerprint of the layout it was decoded with, so a replay can assert the same schema.
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
    package_observations: AtomicU64,
    failures: AtomicU64,
    unrecognised: AtomicU64,
    checkpoints: AtomicU64,
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
            package_observations: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            unrecognised: AtomicU64::new(0),
            checkpoints: AtomicU64::new(0),
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
    /// — so they are loaded deliberately rather than as part of the checkpoint walk.
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
    /// 3. decoding parallelised with `rayon`, because the layouts are already in hand;
    /// 4. slots written last, one object at a time, with the commit counter bumped at the end.
    pub async fn apply_checkpoint<L: LayoutSource + ?Sized>(
        &self,
        layouts: &L,
        checkpoint: &Checkpoint,
    ) -> Result<UpdateReport, StateError> {
        let mut report = UpdateReport {
            checkpoint: checkpoint.summary.sequence_number,
            ..UpdateReport::default()
        };

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

        // (3) Resolve each distinct tag once.
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
            let layout = layouts.layout(tag).await?;
            let fingerprint = birdai_resolve::layout::fingerprint(&layout);
            resolved.insert(tag.clone(), (layout, fingerprint));
        }

        // (4) Decode in parallel: no I/O left, and every layout is in hand.
        let decoded: Vec<DecodeOutcome> = candidates
            .par_iter()
            .map(|(object, tag)| {
                let (layout, fingerprint) = resolved
                    .get(tag)
                    .ok_or_else(|| StateError::Package(tag.to_canonical_string(true)))?;
                let Some(move_object) = object.data.try_as_move() else {
                    return Err(StateError::Package(object.id().to_canonical_string(true)));
                };
                let venue = decode_venue(move_object.contents(), tag, layout)?;
                Ok((object.id(), object.version().value(), tag.clone(), *fingerprint, venue))
            })
            .collect();

        // (5) Publish. Every slot is replaced before the checkpoint counter moves, so a reader
        //     sees either the whole checkpoint or none of it.
        for outcome in decoded {
            match outcome {
                Ok((id, version, tag, layout, venue)) => {
                    let kind = venue.kind();
                    // Tick nodes are loaded deliberately rather than walked, so carry them over.
                    let ticks =
                        self.slots.read_sync(&id, |_key, slot| slot.ticks.clone()).flatten();
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
        }

        for (parent, field_id) in child_parents {
            let inserted = self
                .children
                .entry_sync(parent)
                .or_insert_with(|| ChildSet::new(self.child_capacity))
                .get_mut()
                .insert(field_id);
            if inserted {
                self.children_indexed.fetch_add(1, Ordering::Relaxed);
                report.children_indexed += 1;
            }
        }

        self.checkpoints.fetch_add(1, Ordering::Relaxed);
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
    pub fn install_ticks(
        &self,
        id: ObjectID,
        head: &SkipListHead,
        nodes: impl IntoIterator<Item = birdai_tick::TickNode>,
    ) -> Result<Arc<Ticks>, StateError> {
        let ticks = Ticks::new(Some(head.node_uid), Some(head.size), nodes)?;
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
