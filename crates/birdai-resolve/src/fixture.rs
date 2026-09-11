//! Offline replay: capture everything a run needs, then serve it back with no network.
//!
//! The fixture set is deliberately made of **raw Sui types**, not of derived values:
//!
//! * objects and packages are stored as the BCS of `sui_types::object::Object`,
//! * layouts as `move_core_types::annotated_value::MoveTypeLayout` JSON,
//! * checkpoints as the BCS of their parts, reassembled into
//!   `sui_types::full_checkpoint_content::Checkpoint`.
//!
//! Nothing in the fixture is a pre-computed answer, so an offline run exercises the same decoders,
//! the same layout resolution and the same classifier as an online one. If a fixture run and a
//! mainnet run disagree, the code changed.
//!
//! # What a captured checkpoint is
//!
//! `Checkpoint` is not `Serialize` — Sui asserts that on purpose — so it is stored as its parts:
//! summary, contents, and one entry per captured transaction (transaction, signatures, effects,
//! events, unchanged objects), plus the objects its effects refer to. A capture is **filtered** to
//! the transactions whose effects touch the objects of interest, so the reassembled `object_set` is
//! a subset of the chain's. That is stated in the manifest rather than left implicit, and it is
//! enough for every offline command here, which only ever look up objects those transactions
//! changed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use move_core_types::{
    account_address::AccountAddress, annotated_value::MoveTypeLayout, language_storage::StructTag,
};
use serde::{Deserialize, Serialize};
use sui_package_resolver::{Package, PackageStore, Resolver};
use sui_types::{
    base_types::{ObjectID, SequenceNumber},
    effects::TransactionEffects,
    full_checkpoint_content::{Checkpoint, ExecutedTransaction, ObjectSet},
    messages_checkpoint::{CertifiedCheckpointSummary, CheckpointContents},
    object::Object,
    storage::ObjectKey,
    transaction::TransactionData,
};
use thiserror::Error;

use crate::{error::ResolveError, object::ObjectSource, store::SourcePackageStore};

/// The file names a fixture directory is made of.
pub const OBJECTS_FILE: &str = "objects.json";
/// Packages, as `sui_types::object::Object` BCS.
pub const PACKAGES_FILE: &str = "packages.json";
/// Resolved layouts, as `MoveTypeLayout` JSON.
pub const LAYOUTS_FILE: &str = "layouts.json";
/// Captured checkpoints.
pub const CHECKPOINTS_FILE: &str = "checkpoints.json";
/// Provenance.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Something went wrong reading or writing a fixture set.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// A file could not be read or written.
    #[error("fixture io at {path}: {message}")]
    Io {
        /// The file involved.
        path: String,
        /// The underlying message.
        message: String,
    },

    /// A file was not valid JSON for its type.
    #[error("fixture `{path}` is malformed: {message}")]
    Malformed {
        /// The file involved.
        path: String,
        /// The underlying message.
        message: String,
    },

    /// A stored blob was not valid BCS for the type it claims to be.
    #[error("fixture `{path}` holds invalid BCS for {what}: {message}")]
    Bcs {
        /// The file involved.
        path: String,
        /// What was being decoded.
        what: &'static str,
        /// The underlying message.
        message: String,
    },

    /// The fixture does not contain something the run needs.
    #[error("fixture is missing {what} ({detail}); recapture with `birdai fetch`")]
    Missing {
        /// What was wanted.
        what: &'static str,
        /// Which one.
        detail: String,
    },
}

fn io(path: &Path, error: impl std::fmt::Display) -> FixtureError {
    FixtureError::Io { path: path.display().to_string(), message: error.to_string() }
}

/// Where a fixture set came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// The chain the node reported.
    pub chain: String,
    /// The endpoint used.
    pub rpc_url: String,
    /// Capture time, seconds since the Unix epoch.
    pub captured_at_unix: u64,
    /// Anything a reader needs to know about the capture's limits.
    pub note: String,
}

/// One transaction of a captured checkpoint, as its parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedTransaction {
    /// BCS of `TransactionData`.
    pub transaction: String,
    /// BCS of each signature.
    pub signatures: Vec<String>,
    /// BCS of `TransactionEffects`.
    pub effects: String,
    /// BCS of `TransactionEvents`, when the transaction emitted any.
    pub events: Option<String>,
    /// Unchanged objects the transaction loaded, as `(id, version)`.
    pub unchanged: Vec<(String, u64)>,
}

/// A captured checkpoint: its parts plus the objects its captured transactions refer to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedCheckpoint {
    /// BCS of `CertifiedCheckpointSummary`.
    pub summary: String,
    /// BCS of `CheckpointContents`.
    pub contents: String,
    /// The captured transactions.
    pub transactions: Vec<CapturedTransaction>,
    /// `"<object id>" -> { "<version>" -> base64(Object BCS) }`, one map per object id.
    pub objects: BTreeMap<String, BTreeMap<u64, String>>,
}

/// A fixture set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Fixtures {
    /// Provenance.
    pub manifest: Manifest,
    /// `"<object id>" -> { "<version>" -> base64(Object BCS) }`.
    pub objects: BTreeMap<String, BTreeMap<u64, String>>,
    /// `"<package address>" -> base64(Object BCS)`.
    pub packages: BTreeMap<String, String>,
    /// Canonical `StructTag` -> `MoveTypeLayout` JSON.
    pub layouts: BTreeMap<String, serde_json::Value>,
    /// Non-canonical `StructTag` -> canonical `StructTag`.
    pub canonical: BTreeMap<String, String>,
    /// Captured checkpoints by sequence number.
    pub checkpoints: BTreeMap<u64, CapturedCheckpoint>,
}

/// The files a fixture directory is made of.
#[derive(Debug, Clone)]
pub struct FixturePaths {
    /// Directory holding the files.
    pub root: PathBuf,
}

impl FixturePaths {
    /// The paths under `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Fixtures {
    /// Read a fixture directory.
    ///
    /// A directory that does not exist is an error, not an empty set: silently replaying nothing
    /// would turn a mistyped `--fixtures` path into "object not found" three commands later.
    pub fn load(root: impl Into<PathBuf>) -> Result<Self, FixtureError> {
        let paths = FixturePaths::new(root);
        if !paths.root.join(MANIFEST_FILE).exists() {
            return Err(FixtureError::Missing {
                what: "fixture directory",
                detail: paths.root.display().to_string(),
            });
        }
        Ok(Self {
            manifest: read_json(&paths.file(MANIFEST_FILE))?,
            objects: read_json(&paths.file(OBJECTS_FILE))?,
            packages: read_json(&paths.file(PACKAGES_FILE))?,
            layouts: read_json(&paths.file(LAYOUTS_FILE))?,
            canonical: read_json(&paths.file(CANONICAL_FILE))?,
            checkpoints: read_json(&paths.file(CHECKPOINTS_FILE))?,
        })
    }

    /// Write a fixture directory, creating it if needed.
    pub fn save(&self, root: impl Into<PathBuf>) -> Result<(), FixtureError> {
        let paths = FixturePaths::new(root);
        std::fs::create_dir_all(&paths.root).map_err(|error| io(&paths.root, error))?;
        write_json(&paths.file(MANIFEST_FILE), &self.manifest)?;
        write_json(&paths.file(OBJECTS_FILE), &self.objects)?;
        write_json(&paths.file(PACKAGES_FILE), &self.packages)?;
        write_json(&paths.file(LAYOUTS_FILE), &self.layouts)?;
        write_json(&paths.file(CANONICAL_FILE), &self.canonical)?;
        write_json(&paths.file(CHECKPOINTS_FILE), &self.checkpoints)?;
        Ok(())
    }

    /// Record one object at the version it was read at.
    pub fn record_object(&mut self, object: &Object) -> Result<(), FixtureError> {
        record_into(&mut self.objects, object, "object")
    }

    /// Record a package under the address layouts will reference it by.
    pub fn record_package(
        &mut self,
        address: &AccountAddress,
        object: &Object,
    ) -> Result<(), FixtureError> {
        let bytes = bcs::to_bytes(object).map_err(|error| FixtureError::Bcs {
            path: PACKAGES_FILE.to_owned(),
            what: "Object",
            message: error.to_string(),
        })?;
        self.packages.insert(address.to_canonical_string(true), BASE64.encode(bytes));
        Ok(())
    }

    /// Record a resolved layout and the canonical form of the tag it was resolved for.
    pub fn record_layout(
        &mut self,
        requested: &StructTag,
        canonical: &StructTag,
        layout: &MoveTypeLayout,
    ) -> Result<(), FixtureError> {
        let json = serde_json::to_value(layout).map_err(|error| FixtureError::Malformed {
            path: LAYOUTS_FILE.to_owned(),
            message: error.to_string(),
        })?;
        self.layouts.insert(canonical.to_canonical_string(true), json);
        self.canonical
            .insert(requested.to_canonical_string(true), canonical.to_canonical_string(true));
        Ok(())
    }

    /// Record a checkpoint, keeping only the transactions `keep` accepts.
    pub fn record_checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
        mut keep: impl FnMut(&ExecutedTransaction) -> bool,
    ) -> Result<(), FixtureError> {
        let summary = bcs::to_bytes(&checkpoint.summary).map_err(|error| FixtureError::Bcs {
            path: CHECKPOINTS_FILE.to_owned(),
            what: "CertifiedCheckpointSummary",
            message: error.to_string(),
        })?;
        let contents = bcs::to_bytes(&checkpoint.contents).map_err(|error| FixtureError::Bcs {
            path: CHECKPOINTS_FILE.to_owned(),
            what: "CheckpointContents",
            message: error.to_string(),
        })?;

        let mut captured = CapturedCheckpoint {
            summary: BASE64.encode(summary),
            contents: BASE64.encode(contents),
            transactions: Vec::new(),
            objects: BTreeMap::new(),
        };

        for executed in &checkpoint.transactions {
            if !keep(executed) {
                continue;
            }
            let transaction =
                bcs::to_bytes(&executed.transaction).map_err(|error| FixtureError::Bcs {
                    path: CHECKPOINTS_FILE.to_owned(),
                    what: "TransactionData",
                    message: error.to_string(),
                })?;
            let effects = bcs::to_bytes(&executed.effects).map_err(|error| FixtureError::Bcs {
                path: CHECKPOINTS_FILE.to_owned(),
                what: "TransactionEffects",
                message: error.to_string(),
            })?;
            let events = executed
                .events
                .as_ref()
                .map(bcs::to_bytes)
                .transpose()
                .map_err(|error| FixtureError::Bcs {
                    path: CHECKPOINTS_FILE.to_owned(),
                    what: "TransactionEvents",
                    message: error.to_string(),
                })?
                .map(|bytes| BASE64.encode(bytes));
            let signatures = executed
                .signatures
                .iter()
                .map(|signature| {
                    bcs::to_bytes(signature).map(|bytes| BASE64.encode(bytes)).map_err(|error| {
                        FixtureError::Bcs {
                            path: CHECKPOINTS_FILE.to_owned(),
                            what: "GenericSignature",
                            message: error.to_string(),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Every input and output the effects name, so the reassembled `object_set` can resolve
            // the transaction's own view of the chain.
            for change in
                sui_types::effects::TransactionEffectsAPI::object_changes(&executed.effects)
            {
                for version in [change.input_version, change.output_version].into_iter().flatten() {
                    if let Some(object) = checkpoint.object_set.get(&ObjectKey(change.id, version))
                    {
                        record_into(&mut captured.objects, object, "object")?;
                    }
                }
            }

            captured.transactions.push(CapturedTransaction {
                transaction: BASE64.encode(transaction),
                signatures,
                effects: BASE64.encode(effects),
                events,
                unchanged: executed
                    .unchanged_loaded_runtime_objects
                    .iter()
                    .map(|key| (key.0.to_canonical_string(true), key.1.value()))
                    .collect(),
            });
        }

        self.checkpoints.insert(checkpoint.summary.sequence_number, captured);
        Ok(())
    }

    /// The object at `version`, or at the only version held when `version` is `None`.
    pub fn object(&self, id: &ObjectID, version: Option<u64>) -> Option<Object> {
        let versions = self.objects.get(&id.to_canonical_string(true))?;
        let encoded = match version {
            Some(version) => versions.get(&version)?,
            None => latest(versions)?,
        };
        match decode(encoded) {
            Ok(object) => Some(object),
            // A corrupt entry is a broken capture, not a missing object: say so where the
            // caller can see it instead of surfacing as `ObjectNotFound` three calls later.
            Err(error) => {
                tracing::warn!(object = %id.to_canonical_string(true), %error, "fixture entry failed to decode");
                None
            }
        }
    }

    /// Every version of an object held.
    #[must_use]
    pub fn versions(&self, id: &ObjectID) -> Vec<u64> {
        self.objects
            .get(&id.to_canonical_string(true))
            .map(|versions| versions.keys().copied().collect())
            .unwrap_or_default()
    }

    /// A package, by the address layouts reference it with.
    pub fn package(&self, address: &AccountAddress) -> Option<Object> {
        let encoded = self.packages.get(&address.to_canonical_string(true))?;
        match decode(encoded) {
            Ok(object) => Some(object),
            Err(error) => {
                tracing::warn!(package = %address.to_canonical_string(true), %error, "fixture package failed to decode");
                None
            }
        }
    }

    /// A layout, by canonical tag.
    pub fn layout(&self, canonical: &StructTag) -> Option<MoveTypeLayout> {
        serde_json::from_value(self.layouts.get(&canonical.to_canonical_string(true))?.clone()).ok()
    }

    /// The canonical form of a tag.
    #[must_use]
    pub fn canonical_of(&self, tag: &StructTag) -> Option<String> {
        self.canonical.get(&tag.to_canonical_string(true)).cloned()
    }

    /// Reassemble a captured checkpoint.
    pub fn checkpoint(&self, sequence: u64) -> Result<Checkpoint, FixtureError> {
        let captured = self.checkpoints.get(&sequence).ok_or_else(|| FixtureError::Missing {
            what: "checkpoint",
            detail: sequence.to_string(),
        })?;

        let summary: CertifiedCheckpointSummary =
            decode_required(&captured.summary, CHECKPOINTS_FILE, "CertifiedCheckpointSummary")?;
        let contents: CheckpointContents =
            decode_required(&captured.contents, CHECKPOINTS_FILE, "CheckpointContents")?;

        let mut object_set = ObjectSet::default();
        for versions in captured.objects.values() {
            for encoded in versions.values() {
                match decode(encoded) {
                    Ok(object) => {
                        object_set.insert(object);
                    }
                    // Dropping a corrupt entry silently would shrink the checkpoint's object
                    // set and break input resolution downstream; warn so a bad capture is
                    // loud at load time.
                    Err(error) => {
                        tracing::warn!(%error, "fixture checkpoint entry failed to decode");
                    }
                }
            }
        }

        let mut transactions = Vec::with_capacity(captured.transactions.len());
        for entry in &captured.transactions {
            let transaction: TransactionData =
                decode_required(&entry.transaction, CHECKPOINTS_FILE, "TransactionData")?;
            let effects: TransactionEffects =
                decode_required(&entry.effects, CHECKPOINTS_FILE, "TransactionEffects")?;
            let events = entry
                .events
                .as_deref()
                .map(|encoded| decode_required(encoded, CHECKPOINTS_FILE, "TransactionEvents"))
                .transpose()?;
            let signatures = entry
                .signatures
                .iter()
                .map(|encoded| decode_required(encoded, CHECKPOINTS_FILE, "GenericSignature"))
                .collect::<Result<Vec<_>, _>>()?;
            let unchanged = entry
                .unchanged
                .iter()
                .filter_map(|(id, version)| {
                    id.parse::<ObjectID>()
                        .ok()
                        .map(|id| ObjectKey(id, SequenceNumber::from_u64(*version)))
                })
                .collect();

            transactions.push(ExecutedTransaction {
                transaction,
                signatures,
                effects,
                events,
                unchanged_loaded_runtime_objects: unchanged,
            });
        }

        Ok(Checkpoint { summary, contents, transactions, object_set })
    }
}

/// The canonical-tag map's file name.
pub const CANONICAL_FILE: &str = "canonical.json";

fn latest(versions: &BTreeMap<u64, String>) -> Option<&String> {
    versions.iter().next_back().map(|(_, encoded)| encoded)
}

fn decode(encoded: &str) -> Result<Object, FixtureError> {
    let bytes = BASE64.decode(encoded).map_err(|error| FixtureError::Bcs {
        path: OBJECTS_FILE.to_owned(),
        what: "base64",
        message: error.to_string(),
    })?;
    bcs::from_bytes(&bytes).map_err(|error| FixtureError::Bcs {
        path: OBJECTS_FILE.to_owned(),
        what: "Object",
        message: error.to_string(),
    })
}

fn decode_required<T: serde::de::DeserializeOwned>(
    encoded: &str,
    path: &str,
    what: &'static str,
) -> Result<T, FixtureError> {
    let bytes = BASE64.decode(encoded).map_err(|error| FixtureError::Bcs {
        path: path.to_owned(),
        what: "base64",
        message: error.to_string(),
    })?;
    bcs::from_bytes(&bytes).map_err(|error| FixtureError::Bcs {
        path: path.to_owned(),
        what,
        message: error.to_string(),
    })
}

fn record_into(
    into: &mut BTreeMap<String, BTreeMap<u64, String>>,
    object: &Object,
    what: &'static str,
) -> Result<(), FixtureError> {
    let bytes = bcs::to_bytes(object).map_err(|error| FixtureError::Bcs {
        path: OBJECTS_FILE.to_owned(),
        what,
        message: error.to_string(),
    })?;
    into.entry(object.id().to_canonical_string(true))
        .or_default()
        .insert(object.version().value(), BASE64.encode(bytes));
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T, FixtureError> {
    if !path.exists() {
        // ponytail: missing fixture files default to empty but warn loudly; a mistyped
        // `--fixtures` path must not surface as a far-away `ObjectNotFound`.
        tracing::warn!("fixture file {} is missing; using an empty set", path.display());
        return Ok(T::default());
    }
    let text = std::fs::read_to_string(path).map_err(|error| io(path, error))?;
    serde_json::from_str(&text).map_err(|error| FixtureError::Malformed {
        path: path.display().to_string(),
        message: error.to_string(),
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), FixtureError> {
    let text = serde_json::to_string(value).map_err(|error| FixtureError::Malformed {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    // ponytail: atomic write so a crash cannot leave a truncated JSON that loads as empty.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|error| io(path, error))?;
    std::fs::rename(&tmp, path).map_err(|error| io(path, error))
}

/// An [`ObjectSource`] that serves a fixture set and never touches the network.
#[derive(Debug)]
pub struct FixtureObjectSource {
    fixtures: Arc<Fixtures>,
}

impl FixtureObjectSource {
    /// Serve `fixtures`.
    #[must_use]
    pub const fn new(fixtures: Arc<Fixtures>) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl ObjectSource for FixtureObjectSource {
    async fn object(&self, id: ObjectID, version: Option<u64>) -> Result<Object, ResolveError> {
        self.fixtures.object(&id, version).ok_or_else(|| ResolveError::ObjectNotFound {
            id: id.to_canonical_string(true),
            version,
        })
    }

    async fn checkpoint(&self, sequence_number: u64) -> Result<Checkpoint, ResolveError> {
        self.fixtures
            .checkpoint(sequence_number)
            .map_err(|error| ResolveError::Fixture(Box::new(error)))
    }

    async fn dynamic_fields(
        &self,
        parent: ObjectID,
        _cursor: Option<bytes::Bytes>,
    ) -> Result<crate::object::DynamicFieldPage, ResolveError> {
        // The capture stores the dynamic field *objects*, not the chain's index, so replay
        // reconstructs the parent's children by filtering on the recorded owner — which is exactly
        // what the index would have returned. Single-page: fixtures are a closed set.
        let mut entries = Vec::new();
        for encoded in self.fixtures.objects.values().flat_map(|versions| versions.values()) {
            let object = match decode(encoded) {
                Ok(object) => object,
                Err(error) => {
                    tracing::warn!(%error, "fixture entry failed to decode; skipping");
                    continue;
                }
            };
            let Some(owner) = crate::object::dynamic_field_parent(&object) else { continue };
            if owner == parent {
                entries.push(crate::object::DynamicFieldRef { parent, field_id: object.id() });
            }
        }
        Ok(crate::object::DynamicFieldPage { entries, next: None })
    }

    async fn chain_id(&self) -> Result<String, ResolveError> {
        Ok(self.fixtures.manifest.chain.clone())
    }

    async fn latest_checkpoint(&self) -> Result<u64, ResolveError> {
        Ok(self.fixtures.checkpoints.iter().next_back().map_or(0, |(sequence, _)| *sequence))
    }
}

/// A [`crate::layout::LayoutSource`] that serves a fixture set.
///
/// Layouts are stored resolved, so this performs no resolution work at all — which is the point:
/// an offline run decodes with exactly the layouts that were live when the fixture was captured,
/// and `LayoutRegistry`'s fingerprints can assert it.
#[derive(Debug)]
pub struct FixtureLayoutSource {
    fixtures: Arc<Fixtures>,
    resolver: Resolver<SourcePackageStore<FixtureObjectSource>>,
}

impl FixtureLayoutSource {
    /// Serve `fixtures`.
    #[must_use]
    pub fn new(fixtures: Arc<Fixtures>) -> Self {
        let source = Arc::new(FixtureObjectSource::new(fixtures.clone()));
        Self {
            fixtures,
            resolver: Resolver::new_with_limits(
                SourcePackageStore::new(source),
                crate::LAYOUT_LIMITS,
            ),
        }
    }
}

#[async_trait]
impl crate::layout::LayoutSource for FixtureLayoutSource {
    async fn layout(&self, tag: &StructTag) -> Result<Arc<MoveTypeLayout>, ResolveError> {
        if let Some(canonical) = self.fixtures.canonical_of(tag) &&
            let Ok(canonical) = canonical.parse::<StructTag>() &&
            let Some(layout) = self.fixtures.layout(&canonical)
        {
            return Ok(Arc::new(layout));
        }
        // Not captured: fall back to resolving from the captured packages, which works whenever the
        // bytecode is present even if the layout never got recorded.
        self.resolver
            .type_layout(move_core_types::language_storage::TypeTag::Struct(Box::new(tag.clone())))
            .await
            .map(Arc::new)
            .map_err(|source| ResolveError::Layout { tag: tag.to_canonical_string(true), source })
    }

    async fn canonical(&self, tag: &StructTag) -> Result<StructTag, ResolveError> {
        if let Some(canonical) = self.fixtures.canonical_of(tag) &&
            let Ok(parsed) = canonical.parse::<StructTag>()
        {
            return Ok(parsed);
        }
        let resolved = self
            .resolver
            .canonical_type(move_core_types::language_storage::TypeTag::Struct(Box::new(
                tag.clone(),
            )))
            .await
            .map_err(|source| ResolveError::Layout {
                tag: tag.to_canonical_string(true),
                source,
            })?;
        match resolved {
            move_core_types::language_storage::TypeTag::Struct(inner) => Ok(*inner),
            other => Err(ResolveError::Layout {
                tag: other.to_string(),
                source: sui_package_resolver::error::Error::NotAPackage(AccountAddress::ZERO),
            }),
        }
    }

    async fn package(&self, address: AccountAddress) -> Result<Arc<Package>, ResolveError> {
        let object = self.fixtures.package(&address).ok_or_else(|| {
            ResolveError::ObjectNotFound { id: address.to_canonical_string(true), version: None }
        })?;
        Ok(Arc::new(Package::read_from_object(&object).map_err(|source| ResolveError::Layout {
            tag: address.to_canonical_string(true),
            source,
        })?))
    }
}

/// A [`PackageStore`] over a fixture set, for callers that want a bare resolver.
#[derive(Debug)]
pub struct FixturePackageStore {
    fixtures: Arc<Fixtures>,
}

impl FixturePackageStore {
    /// Serve `fixtures`.
    #[must_use]
    pub const fn new(fixtures: Arc<Fixtures>) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl PackageStore for FixturePackageStore {
    async fn fetch(
        &self,
        id: AccountAddress,
    ) -> Result<Arc<Package>, sui_package_resolver::error::Error> {
        let object = self
            .fixtures
            .package(&id)
            .ok_or(sui_package_resolver::error::Error::PackageNotFound(id))?;
        Ok(Arc::new(Package::read_from_object(&object)?))
    }
}

#[cfg(test)]
mod tests {
    use move_core_types::{
        account_address::AccountAddress, identifier::Identifier, language_storage::StructTag,
    };

    use super::{FixtureError, FixturePaths, Fixtures, MANIFEST_FILE, Manifest, OBJECTS_FILE};

    fn tag(module: &str, name: &str) -> StructTag {
        StructTag {
            address: AccountAddress::TWO,
            module: Identifier::new(module).unwrap_or_else(|_| unreachable!()),
            name: Identifier::new(name).unwrap_or_else(|_| unreachable!()),
            type_params: vec![],
        }
    }

    #[test]
    fn a_missing_directory_is_an_error_not_an_empty_set() -> Result<(), FixtureError> {
        // Silently replaying nothing would turn a mistyped path into a confusing failure later.
        let outcome = Fixtures::load("/nonexistent/fixtures/directory");
        assert!(matches!(outcome, Err(FixtureError::Missing { .. })));
        Ok(())
    }

    #[test]
    fn an_empty_directory_loads_as_an_empty_set() -> Result<(), FixtureError> {
        let dir = tempfile::tempdir().map_err(|error| FixtureError::Io {
            path: "tempdir".to_owned(),
            message: error.to_string(),
        })?;
        // A manifest exists but no data was written yet.
        let manifest = serde_json::to_string(&Manifest::default()).map_err(|error| {
            FixtureError::Io { path: MANIFEST_FILE.to_owned(), message: error.to_string() }
        })?;
        std::fs::write(dir.path().join(MANIFEST_FILE), manifest).map_err(|error| {
            FixtureError::Io { path: MANIFEST_FILE.to_owned(), message: error.to_string() }
        })?;
        let fixtures = Fixtures::load(dir.path())?;
        assert!(fixtures.objects.is_empty());
        assert!(fixtures.layouts.is_empty());
        Ok(())
    }

    #[test]
    fn a_saved_set_round_trips() -> Result<(), FixtureError> {
        let dir = tempfile::tempdir().map_err(|error| FixtureError::Io {
            path: "tempdir".to_owned(),
            message: error.to_string(),
        })?;
        let mut fixtures = Fixtures::default();
        fixtures.manifest.chain = "test-chain".to_owned();
        fixtures.manifest.note = "unit test".to_owned();
        fixtures.record_layout(
            &tag("pool", "Pool"),
            &tag("pool", "Pool"),
            &move_core_types::annotated_value::MoveTypeLayout::U64,
        )?;
        fixtures.save(dir.path())?;

        let loaded = Fixtures::load(dir.path())?;
        assert_eq!(loaded.manifest.chain, "test-chain");
        assert!(loaded.layout(&tag("pool", "Pool")).is_some());
        assert_eq!(
            loaded.canonical_of(&tag("pool", "Pool")),
            Some(tag("pool", "Pool").to_canonical_string(true))
        );
        Ok(())
    }

    #[test]
    fn a_malformed_file_is_reported_with_its_path() -> Result<(), FixtureError> {
        let dir = tempfile::tempdir().map_err(|error| FixtureError::Io {
            path: "tempdir".to_owned(),
            message: error.to_string(),
        })?;
        let manifest = serde_json::to_string(&Manifest::default()).map_err(|error| {
            FixtureError::Io { path: MANIFEST_FILE.to_owned(), message: error.to_string() }
        })?;
        std::fs::write(dir.path().join(MANIFEST_FILE), manifest).map_err(|error| {
            FixtureError::Io { path: MANIFEST_FILE.to_owned(), message: error.to_string() }
        })?;
        std::fs::write(dir.path().join(OBJECTS_FILE), "{ not json").map_err(|error| {
            FixtureError::Io { path: OBJECTS_FILE.to_owned(), message: error.to_string() }
        })?;
        let outcome = Fixtures::load(dir.path());
        assert!(matches!(outcome, Err(FixtureError::Malformed { .. })));
        Ok(())
    }

    #[test]
    fn the_paths_helper_names_every_file() {
        let paths = FixturePaths::new("/tmp/fixtures");
        assert!(paths.file(OBJECTS_FILE).ends_with(OBJECTS_FILE));
    }
}

/// Tests over the fixture set that ships with the repository.
///
/// These are the closest thing to an end-to-end test that needs no network: the fixture directory
/// at the repository root holds real mainnet objects, a real checkpoint and real resolved layouts,
/// and every assertion here replays them. `cargo test` therefore runs without a node, and it fails
/// if someone recaptures the fixtures into a state the decoders no longer understand.
#[cfg(test)]
mod committed {
    use std::{path::PathBuf, sync::Arc};

    use move_core_types::account_address::AccountAddress;
    use sui_types::{
        base_types::ObjectID, digests::TransactionDigest, effects::TransactionEffectsAPI,
        transaction::TransactionDataAPI,
    };

    use super::{FixtureLayoutSource, FixtureObjectSource, Fixtures};
    use crate::{layout::LayoutSource, object::ObjectSource};

    /// The pool the exercise is about.
    const POOL_A: &str = "0x51e883ba7c0b566a26cbc8a94cd33eb0abd418a77cc1e60ad22fd9b1f29cd2ab";
    /// The version transaction T consumed.
    const POOL_A_PRE_VERSION: u64 = 995_150_484;
    /// Transaction T.
    const TX_T: &str = "F53RBSPn84e28FDWnunb7dykGTp7sNpzEnNUxG5h5fe7";
    /// The checkpoint T was executed in.
    const TX_T_CHECKPOINT: u64 = 320_577_815;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures")
    }

    /// Load the committed fixtures, skipping when they are absent.
    ///
    /// Skipping is deliberate: the fixtures are required data, but a checkout without them should
    /// not look like a code failure. `birdai fetch` recreates them.
    fn load() -> Result<Option<Fixtures>, super::FixtureError> {
        let dir = fixtures_dir();
        if !dir.join("manifest.json").exists() {
            // ponytail: visible skip so a fixture-less `cargo test` cannot look like coverage.
            tracing::warn!(
                "SKIPPED fixture tests: no fixture directory at {}; run `cargo run -- fetch` to create it",
                dir.display()
            );
            return Ok(None);
        }
        Ok(Some(Fixtures::load(&dir)?))
    }

    fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, std::io::Error> {
        Ok(tokio::runtime::Runtime::new()?.block_on(future))
    }

    #[test]
    fn the_committed_set_has_every_kind_of_entry() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        assert!(!fixtures.objects.is_empty(), "no objects captured");
        assert!(!fixtures.packages.is_empty(), "no packages captured");
        assert!(!fixtures.layouts.is_empty(), "no layouts captured");
        assert!(!fixtures.checkpoints.is_empty(), "no checkpoints captured");
        Ok(())
    }

    #[test]
    fn pool_a_at_the_version_transaction_t_consumed_decodes() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        let object = fixtures
            .object(&POOL_A.parse::<ObjectID>()?, Some(POOL_A_PRE_VERSION))
            .ok_or("fixture is missing pool A at the version T consumed")?;

        let tag = object.struct_tag().ok_or("pool A is not a Move object")?;
        assert_eq!(tag.module.as_str(), "pool");
        assert_eq!(tag.name.as_str(), "Pool");
        assert_eq!(tag.type_params.len(), 2, "Pool<A, B> has two type parameters");
        assert_eq!(object.version().value(), POOL_A_PRE_VERSION);
        Ok(())
    }

    #[test]
    fn the_captured_checkpoint_reassembles_and_carries_transaction_t() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        let checkpoint = fixtures.checkpoint(TX_T_CHECKPOINT).map_err(|error| error.to_string())?;
        assert!(!checkpoint.transactions.is_empty());

        let digest = TX_T.parse::<TransactionDigest>()?;
        let transaction = checkpoint
            .transactions
            .iter()
            .find(|executed| executed.effects.transaction_digest() == &digest)
            .ok_or_else(|| format!("transaction {TX_T} is not in the captured checkpoint"))?;

        assert!(
            transaction.input_objects(&checkpoint.object_set).count() > 0,
            "the object_set must resolve the transaction's inputs"
        );
        let calls: Vec<String> = transaction
            .transaction
            .move_calls()
            .iter()
            .map(|(_, package, module, function)| {
                format!("{}::{module}::{function}", package.to_canonical_string(true))
            })
            .collect();
        assert!(
            calls.iter().any(|call| call.ends_with("::pool_script_v2::swap_b2a")),
            "the entry the chain used must survive capture; got {calls:?}"
        );
        Ok(())
    }

    #[test]
    fn the_tick_node_children_are_reachable_by_their_inner_uid() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        let source = FixtureObjectSource::new(Arc::new(fixtures.clone()));

        // Find the inner UID through the captured objects rather than hard-coding it, so the test
        // still holds after a recapture.
        let node_uid = fixtures
            .objects
            .keys()
            .map(|id| id.parse::<ObjectID>())
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|id| {
                fixtures
                    .object(id, None)
                    .and_then(|object| crate::object::dynamic_field_parent(&object))
                    .is_some()
            })
            .ok_or("no captured object is a dynamic field")?;

        let parent = fixtures
            .object(&node_uid, None)
            .and_then(|object| crate::object::dynamic_field_parent(&object))
            .ok_or("the node has no parent UID")?;

        let page = block_on(source.dynamic_fields(parent, None))??;
        assert!(!page.entries.is_empty(), "the parent's children are discoverable");
        Ok(())
    }

    #[test]
    fn a_layout_round_trips_out_of_the_fixture_json() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        let pool_a =
            fixtures.object(&POOL_A.parse::<ObjectID>()?, None).ok_or("pool A is not captured")?;
        let tag = pool_a.struct_tag().ok_or("pool A is not a Move object")?;

        let source = FixtureLayoutSource::new(Arc::new(fixtures));
        let layout = block_on(source.layout(&tag))??;
        let canonical = block_on(source.canonical(&tag))??;
        assert_eq!(canonical.address, tag.address);
        assert!(
            matches!(layout.as_ref(), move_core_types::annotated_value::MoveTypeLayout::Struct(_)),
            "a pool is a struct"
        );
        Ok(())
    }

    #[test]
    fn package_bytecode_is_served_offline() -> TestResult {
        let Some(fixtures) = load()? else { return Ok(()) };
        let source = FixtureLayoutSource::new(Arc::new(fixtures.clone()));
        let address = fixtures
            .packages
            .keys()
            .next()
            .ok_or("no packages captured")?
            .parse::<AccountAddress>()?;
        let package = block_on(source.package(address))??;
        assert!(!package.modules().is_empty(), "the package must still carry its modules");
        Ok(())
    }
}
