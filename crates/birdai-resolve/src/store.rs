//! A `PackageStore` on top of any [`ObjectSource`].
//!
//! `sui_rpc_resolver::package_store::RpcPackageStore` does the same job for a fullnode URL, but it
//! builds its own client — so it cannot carry an API key, and it cannot be taught to read from
//! fixtures. Implementing the `PackageStore` trait instead is what the trait is for: the resolver
//! above it is untouched, and every source this crate already supports (gRPC, fixtures, a
//! validator's object store) serves package bytecode as well as objects.
//!
//! There is deliberately **no cache here**. `LayoutRegistry` caches resolved layouts, so the
//! resolver only reaches a store on a cache miss; caching packages here as well would add a second
//! thing to invalidate when a package is upgraded.

use std::sync::Arc;

use async_trait::async_trait;
use move_core_types::account_address::AccountAddress;
use sui_package_resolver::{Package, PackageStore};
use sui_types::base_types::ObjectID;

use crate::{error::ResolveError, object::ObjectSource};

/// Serves package bytecode from an [`ObjectSource`].
#[derive(Debug)]
pub struct SourcePackageStore<O> {
    source: Arc<O>,
}

impl<O> SourcePackageStore<O> {
    /// Wrap an object source.
    pub const fn new(source: Arc<O>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl<O: ObjectSource + 'static> PackageStore for SourcePackageStore<O> {
    async fn fetch(
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
                sui_package_resolver::error::Error::PackageNotFound(id)
            })?;
        Ok(Arc::new(Package::read_from_object(&object)?))
    }
}
