//! Shared wiring: one object source and one layout source per run.
//!
//! Both are trait objects, so the same commands run against a fullnode or against a captured
//! fixture set. That is the whole point of the fixture support: `reproduce --fixtures fixtures/`
//! and `reproduce` differ only in where the bytes come from, and the fact that they agree is what
//! makes the fixture trustworthy.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use birdai_move::{Dump, dump::DumpValue};
use birdai_resolve::{
    GrpcObjectSource, LayoutSource, ObjectSource, RpcLayoutRegistry,
    fixture::{FixtureLayoutSource, FixtureObjectSource, Fixtures},
    layout_registry_over,
};
use move_core_types::{annotated_value::MoveTypeLayout, language_storage::StructTag};
use sui_types::{base_types::ObjectID, full_checkpoint_content::Checkpoint, object::Object};

/// A connected session: either a fullnode or a fixture set.
pub(crate) struct Session {
    /// Where raw objects come from.
    pub(crate) objects: Arc<dyn ObjectSource>,
    /// Where layouts and package bytecode come from.
    pub(crate) layouts: Arc<dyn LayoutSource>,
    /// The concrete registry, when online.
    ///
    /// The trait object above is what commands use; this is the same object with its concrete
    /// type, because cache statistics and the set of resolved layouts are properties of the
    /// registry rather than of the layout-resolving interface.
    registry: Option<Arc<RpcLayoutRegistry>>,
    /// The fixture directory, when running offline.
    fixtures: Option<PathBuf>,
    /// The loaded fixture set, when offline — commands that need to know what was captured
    /// (how many checkpoints, in which order) read it from here rather than from the sources.
    fixture_set: Option<Arc<Fixtures>>,
    /// The endpoint, when running online.
    rpc_url: Option<String>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("fixtures", &self.fixtures)
            .field("rpc_url", &self.rpc_url)
            .finish_non_exhaustive()
    }
}

/// An object together with its resolved layout and rendered dump.
pub(crate) struct Decoded {
    /// The raw object.
    pub object: Object,
    /// Its canonical type.
    pub tag: StructTag,
    /// The layout it was decoded with.
    pub layout: Arc<MoveTypeLayout>,
    /// The annotated dump.
    pub dump: DumpValue,
}

impl Session {
    /// Connect to a fullnode, optionally with an API key header and a separate archival endpoint
    /// for checkpoint reads.
    pub(crate) fn connect(
        rpc_url: &str,
        api_key: Option<&str>,
        checkpoint_url: Option<&str>,
    ) -> eyre::Result<Self> {
        // One source serves objects *and* package bytecode, so a keyed or archived endpoint only
        // has to be described once.
        let source = Arc::new(GrpcObjectSource::with_endpoints(rpc_url, api_key, checkpoint_url)?);
        let registry = Arc::new(layout_registry_over(source.clone()));
        Ok(Self {
            objects: source,
            layouts: registry.clone(),
            registry: Some(registry),
            fixtures: None,
            fixture_set: None,
            rpc_url: Some(rpc_url.to_owned()),
        })
    }

    /// Serve a captured fixture set, making no network calls at all.
    pub(crate) fn offline(dir: &Path) -> eyre::Result<Self> {
        let fixtures = Arc::new(Fixtures::load(dir)?);
        let objects = Arc::new(FixtureObjectSource::new(fixtures.clone()));
        let layouts = Arc::new(FixtureLayoutSource::new(fixtures.clone()));
        Ok(Self {
            objects,
            layouts,
            registry: None,
            fixtures: Some(dir.to_path_buf()),
            fixture_set: Some(fixtures),
            rpc_url: None,
        })
    }

    /// The checkpoint sequence numbers the fixture set holds, in order. Empty when online.
    #[must_use]
    pub(crate) fn fixture_checkpoints(&self) -> Vec<u64> {
        self.fixture_set
            .as_ref()
            .map(|fixtures| fixtures.checkpoints.keys().copied().collect())
            .unwrap_or_default()
    }

    /// The concrete registry, when online.
    #[must_use]
    pub(crate) const fn registry(&self) -> Option<&Arc<RpcLayoutRegistry>> {
        self.registry.as_ref()
    }

    /// The endpoint, when online.
    #[must_use]
    pub(crate) fn rpc_url(&self) -> Option<&str> {
        self.rpc_url.as_deref()
    }

    /// Whether this session is serving fixtures.
    #[must_use]
    pub(crate) const fn is_offline(&self) -> bool {
        self.fixture_set.is_some()
    }

    /// Fetch an object, resolve its layout and dump it field by field.
    pub(crate) async fn decode(&self, id: ObjectID, version: Option<u64>) -> eyre::Result<Decoded> {
        let object = self.objects.object(id, version).await?;
        let tag = object.struct_tag().ok_or_else(|| eyre::eyre!("{id} is not a Move object"))?;
        let layout = self.layouts.layout(&tag).await?;
        let contents = object
            .data
            .try_as_move()
            .ok_or_else(|| eyre::eyre!("{id} has no Move contents"))?
            .contents();
        let dump = birdai_move::decode_value(contents, &layout, Dump::new())?;
        Ok(Decoded { object, tag, layout, dump })
    }

    /// Fetch a whole checkpoint.
    pub(crate) async fn checkpoint(&self, sequence_number: u64) -> eyre::Result<Checkpoint> {
        Ok(self.objects.checkpoint(sequence_number).await?)
    }

    /// Fetch one object at one version, returning it raw.
    pub(crate) async fn object_at(&self, id: ObjectID, version: u64) -> eyre::Result<Object> {
        Ok(self.objects.object(id, Some(version)).await?)
    }
}
