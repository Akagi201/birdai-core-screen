//! A `PackageStore` on top of any [`ObjectSource`].
//!
//! `sui_rpc_resolver::package_store::RpcPackageStore` does the same job for a fullnode URL, but it
//! builds its own client — so it cannot carry an API key, and it cannot be taught to read from
//! fixtures. Implementing the `PackageStore` trait instead is what the trait is for: the resolver
//! above it is untouched, and every source this crate already supports (gRPC, fixtures, a
//! validator's object store) serves package bytecode as well as objects.
//!
//! The store caches what it fetched, keyed by the versions in [`PackageVersions`] — the same
//! evidence the layout cache invalidates on. Without it every layout lookup would hit the network
//! even on a cache hit: `Resolver::canonical_type` fetches every package a tag names before the
//! cache is consulted, so a hit for `Pool<USDC, SUI>` still costs three round trips (the pool
//! package plus both coin packages). With it, a hit costs none.

use std::sync::Arc;

use async_trait::async_trait;
use move_core_types::account_address::AccountAddress;
use scc::HashMap as SccHashMap;
use sui_package_resolver::{Package, PackageStore};
use sui_types::base_types::ObjectID;

use crate::{error::ResolveError, layout::PackageVersions, object::ObjectSource};

/// Serves package bytecode from an [`ObjectSource`], remembering it until a checkpoint says the
/// package moved.
#[derive(Debug)]
pub struct SourcePackageStore<O> {
    source: Arc<O>,
    versions: PackageVersions,
    /// `(version recorded at fetch time, bytecode)`.
    cache: SccHashMap<AccountAddress, (u64, Arc<Package>)>,
}

impl<O: ObjectSource + 'static> SourcePackageStore<O> {
    /// Wrap an object source, with a private version tracker.
    #[must_use]
    pub fn new(source: Arc<O>) -> Self {
        Self::with_versions(source, PackageVersions::new())
    }

    /// Wrap an object source, sharing the observations a [`crate::LayoutRegistry`] invalidates on.
    #[must_use]
    pub fn with_versions(source: Arc<O>, versions: PackageVersions) -> Self {
        Self { source, versions, cache: SccHashMap::new() }
    }

    /// How many packages are currently held.
    #[must_use]
    pub fn cached_packages(&self) -> usize {
        self.cache.len()
    }

    /// The package object fetched from the source.
    async fn fetch_from_source(
        &self,
        id: AccountAddress,
    ) -> Result<Arc<Package>, sui_package_resolver::error::Error> {
        let object_id = ObjectID::from(id);
        let object = self
            .source
            .object(object_id, None)
            .await
            .map_err(|error: ResolveError| {
                tracing::debug!(package = %id.to_canonical_string(true), %error, "package fetch failed");
                // "Not found" means the package is gone; anything else is a transport failure
                // the resolver must be able to tell apart from a missing package, so it keeps
                // the message instead of collapsing both into `PackageNotFound`.
                match error {
                    ResolveError::ObjectNotFound { .. } => {
                        sui_package_resolver::error::Error::PackageNotFound(id)
                    }
                    other => sui_package_resolver::error::Error::Store {
                        store: "SourcePackageStore",
                        error: other.to_string(),
                    },
                }
            })?;
        Ok(Arc::new(Package::read_from_object(&object)?))
    }
}

#[async_trait]
impl<O: ObjectSource + 'static> PackageStore for SourcePackageStore<O> {
    async fn fetch(
        &self,
        id: AccountAddress,
    ) -> Result<Arc<Package>, sui_package_resolver::error::Error> {
        // A cached package is served only while no newer version has been observed for it. With
        // nothing observed yet the version is 0, which is also what gets recorded, so a cold run
        // caches until the first checkpoint that mentions the package.
        let live = self.versions.live(&id);
        if let Some((recorded, package)) =
            self.cache.read_sync(&id, |_key, (recorded, package)| (*recorded, package.clone())) &&
            recorded >= live
        {
            return Ok(package);
        }

        let package = self.fetch_from_source(id).await?;
        self.cache.upsert_sync(id, (live, package.clone()));
        Ok(package)
    }
}
