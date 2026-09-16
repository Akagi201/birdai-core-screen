//! Object and layout resolution against Sui, with package-upgrade awareness.
//!
//! The crate is deliberately thin. Sui already ships the hard parts:
//!
//! * [`sui_package_resolver::Resolver`] turns a `TypeTag` into an annotated
//!   [`move_core_types::annotated_value::MoveTypeLayout`] by reading real on-chain package
//!   bytecode, and canonicalises every struct tag to its defining package;
//! * [`store::SourcePackageStore`] is a `PackageStore` over any [`ObjectSource`], so the same
//!   source serves objects and package bytecode — including fixtures and a validator's object
//!   store, which a URL-bound store could never do;
//! * [`sui_rpc_api::Client`] fetches objects, checkpoints and dynamic fields as native Sui types.
//!
//! What is added here is the piece none of them cover: **layout invalidation on package upgrade**.
//! See [`layout::LayoutRegistry`].
//!
//! # Example
//!
//! ```ignore
//! let objects = GrpcObjectSource::new("https://fullnode.mainnet.sui.io:443")?;
//! let registry = rpc_layout_registry("https://fullnode.mainnet.sui.io:443");
//!
//! let pool = objects.object(POOL_ID, None).await?;
//! let tag = pool.struct_tag().expect("a Move object");
//! let layout = registry.layout(&tag).await?;
//! ```

pub mod error;
pub mod fixture;
pub mod layout;
pub mod object;
pub mod store;

use std::sync::Arc;

pub use error::ResolveError;
pub use layout::{CacheStats, Fingerprint, LayoutRegistry, LayoutSource, PackageVersions};
pub use object::{
    API_KEY_HEADER, DYNAMIC_FIELD_PAGE_SIZE, DynamicFieldPage, DynamicFieldRef, GrpcObjectSource,
    ObjectSource,
};
use sui_package_resolver::Resolver;

use crate::store::SourcePackageStore;

/// A [`LayoutRegistry`] reading package bytecode from a fullnode over gRPC.
pub type RpcLayoutRegistry = LayoutRegistry<SourcePackageStore<GrpcObjectSource>>;

/// The limits applied to layout resolution.
///
/// These mirror what Sui's own indexers use and are far above what any object in scope needs — a
/// Cetus pool's layout is fewer than twenty nodes deep.
pub const LAYOUT_LIMITS: sui_package_resolver::Limits = sui_package_resolver::Limits {
    max_type_argument_depth: 16,
    max_type_argument_width: 16,
    max_type_nodes: 256,
    max_move_value_depth: 64,
};

/// A [`LayoutRegistry`] over any object source.
///
/// The package store and the layout cache share one [`PackageVersions`] tracker, so a checkpoint's
/// observations invalidate both the resolved layout and the bytecode it came from.
pub fn layout_registry_over<O: ObjectSource + 'static>(
    source: Arc<O>,
) -> LayoutRegistry<SourcePackageStore<O>> {
    let versions = PackageVersions::new();
    let store = SourcePackageStore::with_versions(source, versions.clone());
    let resolver = Resolver::new_with_limits(store, LAYOUT_LIMITS);
    LayoutRegistry::with_versions(Arc::new(resolver), versions)
}

/// Build a resolver that reads package bytecode from `url`.
pub fn rpc_layout_registry(url: &str) -> Result<RpcLayoutRegistry, ResolveError> {
    rpc_layout_registry_with_api_key(url, None)
}

/// Build a resolver that reads package bytecode from `url`, with an optional API key.
pub fn rpc_layout_registry_with_api_key(
    url: &str,
    api_key: Option<&str>,
) -> Result<RpcLayoutRegistry, ResolveError> {
    let source = Arc::new(GrpcObjectSource::with_api_key(url, api_key)?);
    Ok(layout_registry_over(source))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use move_core_types::{
        account_address::AccountAddress,
        annotated_value::{MoveFieldLayout, MoveStructLayout, MoveTypeLayout},
        identifier::Identifier,
        language_storage::{StructTag, TypeTag},
    };

    use super::layout::{LayoutRegistry, PackageVersions, collect_dependencies, fingerprint};

    /// The literals these tests use are valid Move identifiers by construction, and `Identifier`
    /// offers no infallible constructor; a failure here would be a bug in the test itself.
    fn ident(text: &str) -> Identifier {
        Identifier::new(text).unwrap_or_else(|_| unreachable!())
    }

    fn tag(module: &str, name: &str) -> StructTag {
        StructTag {
            address: AccountAddress::TWO,
            module: ident(module),
            name: ident(name),
            type_params: vec![],
        }
    }

    fn struct_layout(
        module: &str,
        name: &str,
        fields: Vec<(&str, MoveTypeLayout)>,
    ) -> MoveTypeLayout {
        MoveTypeLayout::Struct(Box::new(MoveStructLayout {
            type_: tag(module, name),
            fields: fields
                .into_iter()
                .map(|(name, layout)| MoveFieldLayout { name: ident(name), layout })
                .collect(),
        }))
    }

    #[test]
    fn dependencies_walk_nested_structs_and_vectors() {
        let nested = struct_layout("coin", "Coin", vec![("value", MoveTypeLayout::U64)]);
        let outer = struct_layout(
            "pool",
            "Pool",
            vec![
                ("coins", MoveTypeLayout::Vector(Box::new(nested))),
                (
                    "param",
                    MoveTypeLayout::Struct(Box::new(MoveStructLayout {
                        type_: StructTag {
                            address: AccountAddress::ONE,
                            module: ident("string"),
                            name: ident("String"),
                            type_params: vec![TypeTag::U8],
                        },
                        fields: vec![],
                    })),
                ),
            ],
        );

        let dependencies = collect_dependencies(&outer, &PackageVersions::new());
        let addresses: Vec<AccountAddress> = dependencies.iter().map(|(a, _)| *a).collect();
        assert!(addresses.contains(&AccountAddress::ONE));
        assert!(addresses.contains(&AccountAddress::TWO));
        // Unobserved packages are recorded as version 0, so the first version seen for them
        // invalidates the entry — the safe direction.
        assert!(dependencies.iter().all(|(_, version)| *version == 0));
    }

    #[test]
    fn observed_versions_are_recorded_against_the_layout() {
        let layout = struct_layout("coin", "Coin", vec![("value", MoveTypeLayout::U64)]);
        let live = PackageVersions::new();
        live.observe(&[(AccountAddress::TWO, 9)]);
        let dependencies = collect_dependencies(&layout, &live);
        assert_eq!(dependencies, vec![(AccountAddress::TWO, 9)]);
    }

    #[test]
    fn fingerprint_ignores_cosmetic_differences_but_sees_real_ones() {
        let a = struct_layout("pool", "Pool", vec![("liquidity", MoveTypeLayout::U128)]);
        let b = struct_layout("pool", "Pool", vec![("liquidity", MoveTypeLayout::U128)]);
        let c = struct_layout("pool", "Pool", vec![("liquidity", MoveTypeLayout::U64)]);
        let d = struct_layout("pool", "Pool", vec![("sqrt_price", MoveTypeLayout::U128)]);

        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c), "a field type changed");
        assert_ne!(fingerprint(&a), fingerprint(&d), "a field name changed");
    }

    #[test]
    fn an_upgraded_package_invalidates_a_dependent_layout() {
        // A registry without a resolver can still exercise the invalidation bookkeeping.
        let registry: LayoutRegistry<sui_package_resolver::PackageStoreWithLruCache<NoStore>> =
            LayoutRegistry::new(Arc::new(sui_package_resolver::Resolver::new(
                sui_package_resolver::PackageStoreWithLruCache::new(NoStore),
            )));

        // Seed the package tracker by hand through the public API used by the checkpoint path.
        registry.note_packages(&[(AccountAddress::TWO, 1)]);
        assert_eq!(registry.package_versions().get(&AccountAddress::TWO), Some(&1));

        // A second, older observation must not move a version backwards.
        registry.note_packages(&[(AccountAddress::TWO, 1)]);
        assert_eq!(registry.package_versions().get(&AccountAddress::TWO), Some(&1));

        // And a newer one must move it forwards.
        registry.note_packages(&[(AccountAddress::TWO, 7)]);
        assert_eq!(registry.package_versions().get(&AccountAddress::TWO), Some(&7));
    }

    #[test]
    fn note_packages_through_the_trait_dispatch_invalidates() {
        // ponytail: regression for the trait-dispatch trap; `apply_checkpoint` calls
        // through `&dyn LayoutSource`, so the trait method must forward to eviction.
        use super::layout::LayoutSource;
        let registry: LayoutRegistry<sui_package_resolver::PackageStoreWithLruCache<NoStore>> =
            LayoutRegistry::new(Arc::new(sui_package_resolver::Resolver::new(
                sui_package_resolver::PackageStoreWithLruCache::new(NoStore),
            )));
        let source: &dyn LayoutSource = &registry;
        source.note_packages(&[(AccountAddress::TWO, 3)]);
        assert_eq!(registry.package_versions().get(&AccountAddress::TWO), Some(&3));
    }

    #[test]
    fn dependencies_include_generic_type_parameters() {
        // ponytail: a phantom `Pool<A>` depends on A's package even when A leaves no field.
        let inner = StructTag {
            address: AccountAddress::ONE,
            module: ident("usdc"),
            name: ident("USDC"),
            type_params: vec![],
        };
        let layout = struct_layout("pool", "Pool", vec![]);
        let mut with_param = match layout {
            MoveTypeLayout::Struct(inner_layout) => inner_layout,
            _ => unreachable!(),
        };
        with_param.type_.type_params = vec![TypeTag::Struct(Box::new(inner))];
        let layout = MoveTypeLayout::Struct(with_param);
        let addresses: Vec<AccountAddress> = collect_dependencies(&layout, &PackageVersions::new())
            .iter()
            .map(|(a, _)| *a)
            .collect();
        assert!(addresses.contains(&AccountAddress::ONE));
        assert!(addresses.contains(&AccountAddress::TWO));
    }

    /// A `PackageStore` that always fails, so the registry's bookkeeping can be tested without a
    /// network or a resolver fixture.
    #[derive(Debug)]
    struct NoStore;

    #[async_trait::async_trait]
    impl sui_package_resolver::PackageStore for NoStore {
        async fn fetch(
            &self,
            id: AccountAddress,
        ) -> Result<Arc<sui_package_resolver::Package>, sui_package_resolver::error::Error>
        {
            Err(sui_package_resolver::error::Error::PackageNotFound(id))
        }
    }
}
