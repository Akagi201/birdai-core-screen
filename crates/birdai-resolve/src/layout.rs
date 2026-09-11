//! Layout resolution, with package-upgrade awareness layered on top of the Sui resolver.
//!
//! [`sui_package_resolver::Resolver`] does the hard part: it reads real package bytecode and
//! returns an **annotated** `MoveTypeLayout` whose struct tags are canonicalised to the package
//! that first defined them. What it does not do is notice that a package was upgraded.
//! `PackageStoreWithLruCache` caches `Package` values and re-fetches them on demand, but it has no
//! hook that says "package P changed, so every layout that mentions P is now a lie".
//!
//! That matters here because a single object can depend on several packages: pool A's layout
//! mentions `0x1eabed72…::pool`, `0x714a63a0…::i32` and `0xbe21a061…::skip_list`. An upgrade to
//! *any* of them changes the layout the pool must be decoded with. [`LayoutRegistry`] closes that
//! gap by recording, for every cached layout, the versions of every package the layout references,
//! and dropping the entry as soon as one of those packages moves.

use std::{
    collections::{BTreeSet, HashMap},
    hash::BuildHasher,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use move_core_types::{
    account_address::AccountAddress,
    annotated_value::{MoveFieldLayout, MoveTypeLayout},
    language_storage::{StructTag, TypeTag},
};
use sui_package_resolver::{Package, PackageStore, Resolver};

use crate::error::ResolveError;

/// A content hash of a resolved layout: every struct tag, field name and type constructor.
///
/// Recorded next to published state so a checkpoint replay can assert it decoded with the same
/// layout that was live at the time.
pub type Fingerprint = [u8; 32];

/// Where layouts come from.
#[async_trait]
pub trait LayoutSource: Send + Sync {
    /// The annotated layout for `tag`, with type parameters substituted and tags canonicalised.
    async fn layout(&self, tag: &StructTag) -> Result<Arc<MoveTypeLayout>, ResolveError>;

    /// The canonical form of `tag`: same type, but naming the packages that defined its structs.
    async fn canonical(&self, tag: &StructTag) -> Result<StructTag, ResolveError>;

    /// The package at `address`, for bytecode-level inspection.
    async fn package(&self, address: AccountAddress) -> Result<Arc<Package>, ResolveError>;

    /// Record package versions observed in a checkpoint.
    ///
    /// This is the hook that turns "a package was published or upgraded" into "the layouts that
    /// mention it are stale". Backends that cannot observe upgrades ignore it; the default is a
    /// no-op so the trait stays usable from fixtures and tests.
    ///
    /// Invalidation is **push-only**: a layout is re-checked against observed versions on read,
    /// but versions only arrive through this hook. A poller that resolves layouts without ever
    /// feeding checkpoints must call it from its own package observations, or it will serve
    /// stale layouts after an upgrade.
    ///
    /// Takes a slice so both the trait dispatch (`&dyn LayoutSource`) and the inherent
    /// `LayoutRegistry::note_packages` share one signature; a generic `impl IntoIterator`
    /// inherent method would NOT override this hook when called through the trait.
    fn note_packages(&self, versions: &[(AccountAddress, u64)]) {
        let _ = versions;
    }
}

/// A resolved layout plus everything needed to know when it stops being true.
#[derive(Debug)]
struct CachedLayout {
    /// The layout itself.
    layout: Arc<MoveTypeLayout>,
    /// `(package, version)` pairs the layout depends on, as observed when it was cached.
    dependencies: Vec<(AccountAddress, u64)>,
    /// Content hash of the layout, for replay assertions.
    fingerprint: Fingerprint,
}

/// How the cache is behaving. Useful as a metric and in tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that had to resolve.
    pub misses: u64,
    /// Entries dropped because a package they depend on moved.
    pub invalidated: u64,
    /// Distinct layouts currently cached.
    pub entries: u64,
}

/// A layout cache that invalidates on package upgrade.
#[derive(Debug)]
pub struct LayoutRegistry<S> {
    resolver: Arc<Resolver<S>>,
    cache: ArcSwap<HashMap<StructTag, Arc<CachedLayout>>>,
    packages: ArcSwap<HashMap<AccountAddress, u64>>,
    hits: AtomicU64,
    misses: AtomicU64,
    invalidated: AtomicU64,
}

impl<S> LayoutRegistry<S> {
    /// Wrap a resolver.
    pub fn new(resolver: Arc<Resolver<S>>) -> Self {
        Self {
            resolver,
            cache: ArcSwap::from_pointee(HashMap::new()),
            packages: ArcSwap::from_pointee(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            invalidated: AtomicU64::new(0),
        }
    }

    /// The underlying resolver, for callers that need bytecode.
    #[must_use]
    pub const fn resolver(&self) -> &Arc<Resolver<S>> {
        &self.resolver
    }

    /// Snapshot of cache behaviour.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            invalidated: self.invalidated.load(Ordering::Relaxed),
            entries: self.cache.load().len() as u64,
        }
    }

    /// The fingerprint recorded for a cached layout, if it is still live.
    ///
    /// Returns `None` when the tag is uncached **or** when a package it depends on has moved
    /// since it was cached; `layout()` applies the same staleness check on read.
    #[must_use]
    pub fn fingerprint(&self, tag: &StructTag) -> Option<Fingerprint> {
        let entry = self.cache.load().get(tag).cloned()?;
        let live = self.packages.load();
        let stale =
            entry.dependencies.iter().any(|(address, version)| is_newer(&live, address, *version));
        if stale { None } else { Some(entry.fingerprint) }
    }

    /// The versions currently believed to be live for each tracked package.
    #[must_use]
    pub fn package_versions(&self) -> HashMap<AccountAddress, u64> {
        let guard = self.packages.load();
        (**guard).clone()
    }

    /// Every layout currently cached, with the tag it was resolved for.
    ///
    /// Used when capturing a fixture set: rather than re-deriving which layouts a run needs, ask
    /// the cache what it actually handed out.
    #[must_use]
    pub fn cached_layouts(&self) -> Vec<(StructTag, Arc<MoveTypeLayout>)> {
        self.cache.load().iter().map(|(tag, entry)| (tag.clone(), entry.layout.clone())).collect()
    }

    /// Record package versions observed in a checkpoint, and evict anything they invalidate.
    ///
    /// Called with the output of `TransactionEffects::published_packages()` plus the versions of
    /// any `MovePackage` objects seen in the change set. Eviction is **transitive by
    /// construction**: a cached layout lists every package it references, so upgrading
    /// `skip_list` drops the Cetus pool's layout even though the pool's own package did not
    /// move.
    ///
    /// ponytail: same signature as `LayoutSource::note_packages` so trait dispatch forwards here.
    pub fn note_packages(&self, versions: &[(AccountAddress, u64)]) {
        Self::record_packages(self, versions);
    }

    fn record_packages(&self, versions: &[(AccountAddress, u64)]) {
        if versions.is_empty() {
            return;
        }

        self.packages.rcu(|current| {
            let mut next = (**current).clone();
            for &(address, version) in versions {
                next.entry(address)
                    .and_modify(|seen| *seen = (*seen).max(version))
                    .or_insert(version);
            }
            Arc::new(next)
        });

        let live = self.packages.load();
        // Count the stale entries from the snapshot, then remove by key: `rcu` retries its
        // closure under contention, so counting inside it would double-count. The count is
        // approximate if another thread mutates the cache concurrently, which is fine for a
        // metric — eviction itself still removes every stale entry the closure sees.
        let dropped = self
            .cache
            .load()
            .values()
            .filter(|entry| {
                entry
                    .dependencies
                    .iter()
                    .any(|(address, version)| is_newer(&live, address, *version))
            })
            .count() as u64;
        self.cache.rcu(|current| {
            let mut next = (**current).clone();
            next.retain(|_, entry| {
                !entry
                    .dependencies
                    .iter()
                    .any(|(address, version)| is_newer(&live, address, *version))
            });
            Arc::new(next)
        });
        self.invalidated.fetch_add(dropped, Ordering::Relaxed);
    }

    /// Evict a single cached layout.
    pub fn invalidate(&self, tag: &StructTag) {
        self.cache.rcu(|current| {
            let mut next = (**current).clone();
            next.remove(tag);
            Arc::new(next)
        });
    }
}

fn is_newer<S: BuildHasher>(
    live: &HashMap<AccountAddress, u64, S>,
    address: &AccountAddress,
    version: u64,
) -> bool {
    live.get(address).is_some_and(|current| *current > version)
}

/// Every package a `StructTag` mentions, including generic type parameters.
fn collect_tag_addresses(tag: &StructTag, out: &mut BTreeSet<AccountAddress>) {
    out.insert(tag.address);
    for param in &tag.type_params {
        collect_type_tag_addresses(param, out);
    }
}

/// Every package a `TypeTag` mentions.
fn collect_type_tag_addresses(tag: &TypeTag, out: &mut BTreeSet<AccountAddress>) {
    match tag {
        TypeTag::Struct(inner) => collect_tag_addresses(inner, out),
        TypeTag::Vector(inner) => collect_type_tag_addresses(inner, out),
        _ => {}
    }
}

#[async_trait]
impl<S: PackageStore> LayoutSource for LayoutRegistry<S> {
    async fn layout(&self, tag: &StructTag) -> Result<Arc<MoveTypeLayout>, ResolveError> {
        let canonical = self.canonical(tag).await?;

        {
            let cache = self.cache.load();
            if let Some(entry) = cache.get(&canonical) {
                let live = self.packages.load();
                let stale = entry
                    .dependencies
                    .iter()
                    .any(|(address, version)| is_newer(&live, address, *version));
                if !stale {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(entry.layout.clone());
                }
            }
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        let layout =
            self.resolver.type_layout(TypeTag::Struct(Box::new(canonical.clone()))).await.map_err(
                |source| ResolveError::Layout { tag: canonical.to_canonical_string(true), source },
            )?;

        let dependencies = collect_dependencies(&layout, &self.packages.load());
        let fingerprint = fingerprint(&layout);
        let layout = Arc::new(layout);

        let entry = Arc::new(CachedLayout { layout: layout.clone(), dependencies, fingerprint });
        self.cache.rcu(|current| {
            let mut next = (**current).clone();
            next.insert(canonical.clone(), entry.clone());
            Arc::new(next)
        });

        Ok(layout)
    }

    async fn canonical(&self, tag: &StructTag) -> Result<StructTag, ResolveError> {
        let canonical =
            self.resolver.canonical_type(TypeTag::Struct(Box::new(tag.clone()))).await.map_err(
                |source| ResolveError::Layout { tag: tag.to_canonical_string(true), source },
            )?;
        match canonical {
            TypeTag::Struct(inner) => Ok(*inner),
            other => Err(ResolveError::Layout {
                tag: format!("{other} is not a struct"),
                source: sui_package_resolver::error::Error::NotAPackage(AccountAddress::ZERO),
            }),
        }
    }

    async fn package(&self, address: AccountAddress) -> Result<Arc<Package>, ResolveError> {
        self.resolver.package_store().fetch(address).await.map_err(|source| ResolveError::Layout {
            tag: address.to_canonical_string(true),
            source,
        })
    }

    fn note_packages(&self, versions: &[(AccountAddress, u64)]) {
        // ponytail: forward trait dispatch to the shared eviction logic.
        Self::record_packages(self, versions);
    }
}

/// Every package a layout references, paired with the version believed to be live for it.
///
/// This is the dependency set used for invalidation. Because the resolver canonicalises tags to
/// their *defining* package, a struct re-exported through an upgrade chain lists the original
/// package, which is exactly the package whose contents change the layout.
///
/// A package with no recorded version is stored as `0`, meaning "not yet observed"; the layout is
/// then treated as stale the first time a version for it is seen, which is the safe direction.
#[must_use]
pub fn collect_dependencies<S: BuildHasher>(
    layout: &MoveTypeLayout,
    live: &HashMap<AccountAddress, u64, S>,
) -> Vec<(AccountAddress, u64)> {
    let mut addresses = BTreeSet::new();
    visit(layout, &mut |node| {
        if let MoveTypeLayout::Struct(inner) = node {
            collect_tag_addresses(&inner.type_, &mut addresses);
        } else if let MoveTypeLayout::Enum(inner) = node {
            collect_tag_addresses(&inner.type_, &mut addresses);
        }
    });
    addresses
        .into_iter()
        .map(|address| (address, live.get(&address).copied().unwrap_or(0)))
        .collect()
}

/// Hash a layout into a stable fingerprint.
#[must_use]
pub fn fingerprint(layout: &MoveTypeLayout) -> Fingerprint {
    let mut hasher = blake3::Hasher::new();
    hash_into(&mut hasher, layout);
    *hasher.finalize().as_bytes()
}

fn hash_into(hasher: &mut blake3::Hasher, layout: &MoveTypeLayout) {
    match layout {
        MoveTypeLayout::Bool => {
            hasher.update(b"bool");
        }
        MoveTypeLayout::U8 => {
            hasher.update(b"u8");
        }
        MoveTypeLayout::U16 => {
            hasher.update(b"u16");
        }
        MoveTypeLayout::U32 => {
            hasher.update(b"u32");
        }
        MoveTypeLayout::U64 => {
            hasher.update(b"u64");
        }
        MoveTypeLayout::U128 => {
            hasher.update(b"u128");
        }
        MoveTypeLayout::U256 => {
            hasher.update(b"u256");
        }
        MoveTypeLayout::Address => {
            hasher.update(b"address");
        }
        MoveTypeLayout::Signer => {
            hasher.update(b"signer");
        }
        MoveTypeLayout::Vector(inner) => {
            hasher.update(b"vector(");
            hash_into(hasher, inner);
            hasher.update(b")");
        }
        MoveTypeLayout::Struct(inner) => {
            hasher.update(b"struct(");
            hash_tag(hasher, &inner.type_);
            hash_fields(hasher, &inner.fields);
            hasher.update(b")");
        }
        MoveTypeLayout::Enum(inner) => {
            hasher.update(b"enum(");
            hash_tag(hasher, &inner.type_);
            for ((name, tag), fields) in &inner.variants {
                hasher.update(name.as_str().as_bytes());
                hasher.update(&tag.to_le_bytes());
                hash_fields(hasher, fields);
            }
            hasher.update(b")");
        }
    }
}

fn hash_fields(hasher: &mut blake3::Hasher, fields: &[MoveFieldLayout]) {
    for field in fields {
        hasher.update(field.name.as_str().as_bytes());
        hasher.update(b":");
        hash_into(hasher, &field.layout);
        hasher.update(b",");
    }
}

fn hash_tag(hasher: &mut blake3::Hasher, tag: &StructTag) {
    hasher.update(tag.address.as_ref());
    hasher.update(tag.module.as_str().as_bytes());
    hasher.update(b"::");
    hasher.update(tag.name.as_str().as_bytes());
    hasher.update(b"<");
    for param in &tag.type_params {
        hash_type_tag(hasher, param);
    }
    hasher.update(b">");
}

fn hash_type_tag(hasher: &mut blake3::Hasher, tag: &TypeTag) {
    match tag {
        TypeTag::Vector(inner) => {
            hasher.update(b"vector(");
            hash_type_tag(hasher, inner);
            hasher.update(b")");
        }
        TypeTag::Struct(inner) => hash_tag(hasher, inner),
        other => {
            hasher.update(other.to_string().as_bytes());
        }
    }
}

fn visit(layout: &MoveTypeLayout, f: &mut impl FnMut(&MoveTypeLayout)) {
    f(layout);
    match layout {
        MoveTypeLayout::Vector(inner) => visit(inner, f),
        MoveTypeLayout::Struct(inner) => {
            for field in &inner.fields {
                visit(&field.layout, f);
            }
        }
        MoveTypeLayout::Enum(inner) => {
            for fields in inner.variants.values() {
                for field in fields {
                    visit(&field.layout, f);
                }
            }
        }
        _ => {}
    }
}
