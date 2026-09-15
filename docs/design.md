# Birdai Core Screen — Design (rev 2)

**Decode · Classify · Recreate · Keep state current** for three Sui mainnet shared objects, built as a
thin, well-factored layer on top of the Sui crates rather than a reimplementation of them.

> **rev 2 change of direction.** rev 1 proposed a "zero Sui dependency" core with hand-written BCS,
> layout IR, cursor and projection. That was **wrong**, and §9 documents the twelve defects found when the
> design was re-reviewed against the actual Sui source. The conclusion: *reuse the Sui crates for
> everything they already do well; own only what the exercise actually grades.*
>
> **All numbers below are verified against Sui mainnet.** The swap reproduction matches on-chain
> **exactly** (Δ = 0 base units).

---

## 0. TL;DR

| Graded item | Implementation | Evidence |
|---|---|---|
| Decode A/B/C + a tick child | `move_core_types::annotated_visitor` visitors + `annotated_extractor` + `sui_package_resolver::Resolver` | `cargo run -- decode` |
| Classify | Structural bytecode probe over `FunctionDef`/`OpenSignature` + empirical price-state probe | `cargo run -- classify` |
| Recreate T | `birdai-amm` integer CLMM math on the pre-state | `cargo run -- reproduce` → **81 168 759, matches chain** |
| Design note | `birdai-state` on `sui-indexer-alt-framework::ingestion` | `cargo run -- follow --from 320577815` + README §4 |

Pinned upstream: Sui `main` @ **`f0831497799964f2e364a20a380963fc6b4872c5`**
(was `b0535f1f3a3310e71790e90d8ae4e8ca840c897e` at research time; bumped as one atomic move),
toolchain **1.96.1**, edition 2024. `just deps-check` enforces the pin: every git dependency
names a `rev`, and all revs are identical (verified — the whole workspace moves atomically).

---

## 1. Verified on-chain facts

Fetched from `https://graphql.mainnet.sui.io/graphql` **and** gRPC v2, decoded with the resolved layout.

### 1.1 Object A — Cetus `Pool<USDC, SUI>`

```
0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb::pool::Pool<
  0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC,
  0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>
```

Field order (BCS order, 770 bytes total):

| # | field | layout |
|---|---|---|
| 1 | `id` | `0x2::object::UID { id: ID { bytes: address } }` |
| 2 | `coin_a` | `0x2::balance::Balance<USDC> { value: u64 }` |
| 3 | `coin_b` | `0x2::balance::Balance<SUI> { value: u64 }` |
| 4 | `tick_spacing` | `u32` |
| 5 | `fee_rate` | `u64` |
| 6 | `liquidity` | `u128` |
| 7 | `current_sqrt_price` | `u128` |
| 8 | `current_tick_index` | `0x714a63a0…::i32::I32 { bits: u32 }` |
| 9–10 | `fee_growth_global_a/b` | `u128` |
| 11–12 | `fee_protocol_coin_a/b` | `u64` |
| 13 | `tick_manager` | `…::tick::TickManager` |
| 14 | `rewarder_manager` | `…::rewarder::RewarderManager` |
| 15 | `position_manager` | `…::position::PositionManager` |
| 16 | `is_pause` | `bool` |
| 17 | `index` | `u64` |
| 18 | `url` | `0x1::string::String { bytes: vector<u8> }` |

`TickManager { tick_spacing: u32, ticks: 0xbe21a061…::skip_list::SkipList<Tick> }`

```
SkipList<Tick> { id: UID, head: vector<OptionU64>, tail: OptionU64,
                 level: u64, max_level: u64, list_p: u64, size: u64, random: Random { seed: u64 } }
Node<Tick>     { score: u64, nexts: vector<OptionU64>, prev: OptionU64, value: Tick }
Tick           { index: I32, sqrt_price: u128, liquidity_net: I128, liquidity_gross: u128,
                 fee_growth_outside_a: u128, fee_growth_outside_b: u128,
                 points_growth_outside: u128, rewards_growth_outside: vector<u128> }
```

Note `I32` lives in a **different package** from the pool
(`0x714a63a0dba6da4f017b42d5d0fb78867f18bcde904868e51d951a5a6f5b7f57`), and the skip list in a third
(`0xbe21a061…`). Three packages, three upgrade clocks — see §8.5.

Latest state (version 996382523, observed at research time — the pool trades continuously, so
live runs sit higher): `coin_a = 254_548_174_454`, `coin_b = 551_726_244_467_576`,
`liquidity = 68_693_527_635_052`, `current_sqrt_price = 673_624_336_522_390_633_733`,
`current_tick_index = 71959`. The captured fixture set holds 653 tick nodes for the current
version (which declares 654); the pre-T version declares 650.

### 1.2 Fixed-point format — derived, not assumed

`current_sqrt_price` is **Q64.64** (√P scaled by 2^64):

```
S / 2^64          = 35.090681033302715
1.0001^(71162/2)  = 35.09020763740557        (pre-T state, tick 71162)
S / 2^64          = 36.514260…  vs  1.0001^(71959/2)   (latest state)          ✓
```

### 1.3 Transaction T

`F53RBSPn84e28FDWnunb7dykGTp7sNpzEnNUxG5h5fe7`, checkpoint `320577815`, single swap on A.
Pool **input version `995150484`** (digest `Gfpy9tJgDLi3UjAQZa4pgN8XqRPJSxDxqBQKM5qWsvG4`), output
version `995150494`. `atCheckpoint: 320577814` resolves to exactly the input version.

```
coin_a (USDC)       = 325_996_934_562
coin_b (SUI)        = 458_377_448_572_119
liquidity        L  = 120_115_891_674_982
current_sqrt_price S= 647_308_812_393_509_050_120       (= 35.090681033302715 × 2^64)
current_tick_index  = 71162          tick_spacing = 10          fee_rate = 500 (5 bps)
```

Initialised ticks bracketing `71162`:

| tick | sqrt_price | role |
|---:|---:|---|
| 71050 | 643_685_510_299_636_945_792 | below |
| **71060** | 644_007_417_429_774_971_181 | **lower bound of the active range** |
| 71162 | — | current (not on the grid; spacing is 10) |
| ~~**71180**~~ | ~~647_882_882_935_015_212_980~~ | ~~upper bound at research time — since burned~~ |
| **71190** | 648_206_889_171_250_166_865 | **upper bound / next initialised tick** |

Active range **[71060, 71190)**, active liquidity `L = 120_115_891_674_982`.
(The `71180` tick existed at research time and is gone now — live proof that the tick set can
only be read at its current version, and that the quote must not depend on it.)

### 1.4 Reproduction

```
fee               = 100_000_000_000 × 500 / 1e6 = 50_000_000        (matches the stated fee)
amount_in_after_fee = 99_950_000_000
ΔS   = ⌊amount_in_after_fee · 2^64 / L⌋ = 15_349_776_323_987_364
S'   = S + ΔS = 647_324_162_169_833_037_484
out  = ⌊ (L ≪ 64) · ΔS / (S · S') ⌋ = 81_168_759     ← chain: 81_168_759, Δ = 0
```

`S' = 647_324_162_169_833_037_484 < sqrt_price(71190) = 648_206_889_171_250_166_865`
→ **no tick crossing; one step; `L` constant.** In tick units the move is ≈ 0.47 tick.
(`S'` is below the research-time `sqrt_price(71180)` a fortiori.)

### 1.5 Objects B and C

**B — Volo `NativePool`** `0x549e8b69…::native_pool::NativePool`:
`id`, `pending { id, balance: Balance<SUI> }`, `collectable_fee { id, balance }`,
`validator_set { id, vaults: Table{ id, size: 5 }, validators: 0x2::vec_map::VecMap { contents: [(address, u64)] }, sorted_validators: vector<address>, … }`.

**C — Navi `Storage`** `0xd899cf7d…::storage::Storage` (155 bytes in the captured set; 208 at research time):
`id`, `version: u64 = 16`, `paused: bool = false`,
`reserves: 0x2::table::Table<u8, ReserveData> { id, size: 35 }`, `reserves_count: u8 = 35`,
`users: vector<address>`, `user_info: 0x2::table::Table<address, UserInfo> { id, size: 999_105 }`.
All real content is in children — the canonical "a `Table` field is two words of metadata" example.

---

## 2. Dependency strategy

**Rule: take the Sui crates from git at a pinned rev, with default features off wherever that drops
database/systemd weight.**

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.96.1"           # must match the Sui workspace

# Cargo.toml  [workspace.dependencies]
SUI_REV = "f0831497799964f2e364a20a380963fc6b4872c5"   # was b0535f1f… at research time; bumped as one atomic move
sui-types                 = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5" }
sui-package-resolver      = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5" }
sui-rpc-api               = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5" }
sui-indexer-alt-framework = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5", default-features = false }
move-core-types           = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5" }
move-binary-format        = { git = "https://github.com/MystenLabs/sui", rev = "f0831497799964f2e364a20a380963fc6b4872c5" }
```

`move-core-types` and `move-binary-format` live in the same repository
(`external-crates/move/crates/…`) and are workspace members, so a single git source covers everything;
`Cargo.lock` pins every transitive dependency for reproducibility (note: the workspace
template `.gitignore`s the lock file, so a release that must reproduce bit-for-bit should
commit it explicitly).

### Why `default-features = false` on the framework

`sui-indexer-alt-framework` declares `default = ["cluster"]` → `cluster = ["postgres", "dep:tracing-subscriber"]`
→ `postgres = ["dep:sui-pg-db", "dep:diesel", "dep:diesel-async", "dep:diesel_migrations"]`. The Sui
workspace itself pins it as `default-features = false`, so we do the same. The `ingestion` module is
**not** feature-gated, so we keep the whole checkpoint ingestion stack and drop Diesel entirely.

### Build ergonomics (already in this repo, keep)

* `.cargo/config.toml` → `rustc-wrapper = "kache"` (verified present), `pipelining = true`.
* `[profile.dev.package."*"] opt-level = 2` so the decoded hot path is not 20× slow in `cargo run`.
* `[profile.release] lto = "fat"`, `codegen-units = 1`, `strip = "symbols"` (already present).
* `just deps-check` → verifies every git dep is pinned by `rev` (not `branch`) and that all revs are
  identical, so the whole workspace moves to a new Sui rev atomically.

### Optional offline escape hatch

Every number in the README is reproducible without network from a committed fixture set
(`fixtures/objects/*.bcs`, `fixtures/layouts/*.json`, `fixtures/checkpoint-320577815.bin`), served by a
`FixturePackageStore`/`FixtureObjectSource`. `cargo run -- … --offline` never touches a node. Committing a
full `cargo vendor` tree of the Sui dependency graph is deliberately **not** done (multi-GB); the first
build requires network, which is normal for git dependencies and is called out in the README.

---

## 3. Reuse map — what Sui gives us, what we own, and why

### 3.1 Reused (do not reimplement)

| Need | Reused API | Location |
|---|---|---|
| Fetch object by id / version | `sui_rpc_api::Client::{get_object, get_object_with_version, get_object_with_json, batch_get_objects}` → `sui_types::object::Object` | `crates/sui-rpc-api/src/client/mod.rs:120,124,144,185` |
| Fetch checkpoint / tx | `Client::get_full_checkpoint(seq) -> Checkpoint`, `Client::get_transaction(digest) -> ExecutedTransaction`, `Client::get_latest_checkpoint()` | same, `:98,318,63` |
| List dynamic fields | `Client::get_dynamic_fields(parent: ObjectID, page_size, page_token) -> ListDynamicFieldsResponse` | `:421` |
| Layout resolution | `sui_package_resolver::Resolver::new_with_limits(store, limits)` → `Resolver::type_layout(TypeTag) -> MoveTypeLayout` | `crates/sui-package-resolver/src/lib.rs:404` |
| Package bytecode for any source | `birdai_resolve::store::SourcePackageStore<O: ObjectSource>` implements `PackageStore` over the same source that serves objects — gRPC, fixtures, or a validator's object store | `crates/birdai-resolve/src/store.rs` |
| Bytecode / signatures | `Resolver::package_store().fetch(addr) -> Arc<Package>`; `Package::module(name) -> Module`; `Module::{functions, function_def} -> FunctionDef` | `lib.rs:305,749,1059,1076` |
| Type canonicalisation | `Resolver::canonical_type(TypeTag) -> TypeTag`, `Resolver::abilities(TypeTag)` | `lib.rs:382,432` |
| BCS → typed, single pass | `move_core_types::annotated_visitor::{Visitor, Traversal, ValueDriver, StructDriver, VecDriver, VariantDriver, NullTraversal, visitor_default!}` | `…/move-core-types/src/annotated_visitor.rs` |
| Path projection | `move_core_types::annotated_extractor::{Extractor, Element::{Field, Index, Type, Variant}}` | `…/annotated_extractor.rs` |
| BCS → annotated tree (dumps) | `sui_types::object::bounded_visitor::BoundedVisitor::{deserialize_value, deserialize_struct}` | `crates/sui-types/src/object/bounded_visitor.rs:81,96` |
| BCS → JSON (research cross-check) | The node's `contents { json }` over GraphQL, compared by eye during research — deliberately not a dependency: no GraphQL client ships in this repo | Appendix A |
| Zero-copy dynamic field | `sui_types::dynamic_field::visitor::FieldVisitor` → `Field { name_bytes: &'b [u8], value_bytes: &'b [u8], .. }` | `crates/sui-types/src/dynamic_field/visitor.rs:19` |
| Dynamic field id derivation | `sui_types::dynamic_field::{derive_dynamic_field_id, Field}` | `crates/sui-types/src/dynamic_field.rs:269,40` |
| 256-bit integer math | `move_core_types::u256::U256` (`checked_mul`, `checked_div`, `TryFrom<U256> for u128`) | `…/move-core-types/src/u256.rs:280-454` |
| Checkpoint streaming (DB-free) | `sui_indexer_alt_framework::ingestion::{IngestionService, ClientArgs, IngestionConfig, CheckpointEnvelope, GrpcStreamingClient}` | `crates/sui-indexer-alt-framework/src/ingestion/mod.rs` |
| Effects / change sets | `sui_types::effects::TransactionEffectsAPI` (`object_changes`, `modified_at_versions`, `created/mutated/deleted/wrapped/unwrapped`, `published_packages`) | `crates/sui-types/src/effects/mod.rs:310-401` |
| Checkpoint in-memory shapes | `Checkpoint { summary, contents, transactions, object_set }`, `ExecutedTransaction::{input_objects, output_objects, created_objects}`, `ObjectSet` | `crates/sui-types/src/full_checkpoint_content.rs:203-405` |
| Package upgrade metadata | `sui_types::move_package::{MovePackage, TypeOrigin, UpgradeInfo}`, `Effects::published_packages()` | `crates/sui-types/src/move_package.rs` |
| Object / type plumbing | `Object::{id, version, struct_tag, type_, digest}`, `MoveObject::{contents, type_}`, `MoveObjectType → StructTag` | `crates/sui-types/src/object.rs`, `base_types.rs:651` |

### 3.2 Owned — six focused crates

Everything here is either absent from the Sui crates or is the graded answer.

| Crate | Why it cannot be reused |
|---|---|
| `birdai-amm` | **No CLMM / AMM math anywhere in the Sui repo.** It is the graded number. |
| `birdai-tick` | Cetus skip-list layout and `score = tick_index + 443636` bias are protocol-specific. |
| `birdai-venue` | Venue semantics + the classification judgement: the graded reasoning. |
| `birdai-state` | `sui_indexer_alt_framework::pipeline::*` requires a `Store` (Diesel/Postgres or a custom impl) and is DB-shaped; the exercise asks for in-memory typed state. |
| `birdai-move` | Protocol newtypes (`I32`, `I128`, `OptionU64`), the `StructDecoder` trait that turns a layout-driven visitor into a typed struct, and the `Dump` renderer. |
| `birdai-resolve` | `PackageStoreWithLruCache` has **no upgrade hook**: it re-fetches a package only when the package object itself is fetched again, and never invalidates a *dependent* type's cached layout. Package upgrades are one of the three hard problems in task 4. |

`bin/birdai` is the CLI.

---

## 4. Architecture

```
                          bin/birdai   (clap: decode | classify | reproduce | follow | calibrate | check)
                                |
        +-----------------------+------------------------+
        |                       |                        |
  birdai-state            birdai-venue             birdai-resolve
  (checkpoint → typed)    (Venue, Classifier)      (upgrade-aware layout cache)
        |                       |                        |
        |              +--------+--------+               |
        |              |                 |               |
        |        birdai-tick       birdai-amm           |
        |        (skip list)       (CLMM math)          |
        |              |                 |               |
        +--------------+--------+--------+---------------+
                                |
                          birdai-move
                  (I32/I128/OptionU64, StructDecoder, Dump,
                   reusable visitors, dump renderer)
                                |
        =========== Sui crates (pinned git rev) ===========
   sui-types · sui-package-resolver · sui-rpc-api
   sui-indexer-alt-framework · move-core-types · move-binary-format
```

Dependency edges only point downward; `birdai-amm` has **no I/O and no Sui dependency** beyond
`move-core-types::u256` (it is pure integer math and can be property-tested in microseconds).

### 4.1 The seams (as built)

```rust
/// Where raw objects come from. Implemented by the gRPC client and by fixtures.
#[async_trait]
pub trait ObjectSource: Send + Sync {
    async fn object(&self, id: ObjectID, version: Option<u64>) -> Result<Object, ResolveError>;
    async fn checkpoint(&self, sequence_number: u64) -> Result<Checkpoint, ResolveError>;
    async fn dynamic_fields(&self, parent: ObjectID, cursor: Option<Bytes>)
        -> Result<DynamicFieldPage, ResolveError>;
    async fn chain_id(&self) -> Result<String, ResolveError>;
    async fn latest_checkpoint(&self) -> Result<u64, ResolveError>;
    // `objects(&[ObjectID])`: batched with a bounded-concurrency fallback. The default is
    // sequential; the gRPC backend overrides it.
}

/// Where layouts come from, with package-upgrade awareness layered on top.
#[async_trait]
pub trait LayoutSource: Send + Sync {
    async fn layout(&self, tag: &StructTag) -> Result<Arc<MoveTypeLayout>, ResolveError>;
    async fn canonical(&self, tag: &StructTag) -> Result<StructTag, ResolveError>;
    async fn package(&self, address: AccountAddress) -> Result<Arc<Package>, ResolveError>;
    // `note_packages(versions)`: the upgrade hook; a no-op by default.
}

/// How a typed venue is produced from bytes + layout.
pub trait Venue: Sized + Send + Sync + 'static {
    const KIND: VenueKind;
    fn decode(bytes: &[u8], layout: &MoveTypeLayout) -> Result<Self, VenueError>;
    fn price_state(&self) -> Option<PriceState>;      // powers classification probe 2 and pricing
}
```

There is no separate raw-object trait: the checkpoint object **is** `sui_types::object::Object`,
so the boundary is `Object` → `Venue`, with bytes/tag/version as the only inputs. §8.4 shows what
changes when the source is a validator.

---

## 5. Task 1 — Decode

### 5.1 Fetch

Primary path is **gRPC v2** (`sui_rpc_api::Client`), not GraphQL:

```rust
let mut client = sui_rpc_api::Client::new("https://fullnode.mainnet.sui.io:443")?;
let obj: sui_types::object::Object = client.get_object_with_version(pool_id, version)?.into();
let mv = obj.data.try_as_move().ok_or_else(|| eyre::eyre!("not a Move object"))?;
let tag: StructTag = obj.struct_tag().ok_or_else(|| eyre::eyre!("no type tag"))?;
// contents: &[u8] == mv.contents()
```

Why gRPC over GraphQL: it returns a **native `sui_types::object::Object`** (the same type the checkpoint
stream carries), it supports read masks, batching and dynamic-field paging, and `SourcePackageStore`
wraps the same client as a `PackageStore`. GraphQL was used during research as a cross-check channel
(its `contents { json }` is the "pre-parsed JSON" the task allows for verification) and to confirm
`Address.dynamicFields` reaches inner UIDs — no GraphQL client ships in this repo.

### 5.2 Layout

```rust
let source = Arc::new(GrpcObjectSource::with_endpoints(rpc_url, api_key, archive_url)?);
let registry = layout_registry_over(source); // LayoutRegistry<SourcePackageStore<GrpcObjectSource>>
let layout: Arc<MoveTypeLayout> = registry.layout(&pool_tag).await?;
```

The resolver underneath is `sui_package_resolver::Resolver::new_with_limits` with
`Limits { max_type_argument_depth: 16, max_type_argument_width: 16, max_type_nodes: 256,
max_move_value_depth: 64 }` (see `LAYOUT_LIMITS`); the registry adds the upgrade-aware cache
on top.

Two properties of `Resolver::type_layout` that drive the rest of the design:

1. The returned layout is **always the annotated form**
   (`annotated_value::MoveStructLayout { type_, fields: Vec<MoveFieldLayout> }` with field names). The old
   `MoveStructLayout::{Runtime, WithFields, WithTypes}` variants do not exist in this revision.
2. Struct tags inside the layout are **canonicalised to the defining package**, and `TypeTag`s are
   substituted, so `Balance<USDC>` and `Balance<SUI>` come back as two distinct layouts for free.

### 5.3 Typed decode: implement `Visitor`, don't build a tree

Instead of decoding to an intermediate `MoveValue` and then converting, `birdai-move` implements
`move_core_types::annotated_visitor::Visitor` directly for each venue. The driver hands us field names via
`StructDriver::peek_field()`, so fields are matched **by name**, and the framework auto-skips anything we
do not claim (`next_field`/`skip_field`, drained on return).

```rust
impl<'b, 'l> Visitor<'b, 'l> for CetusPoolVisitor {
    type Value = CetusPool;
    type Error = DecodeError;

    fn visit_struct(&mut self, d: &mut StructDriver<'_, 'b, 'l>) -> Result<Self::Value, Self::Error> {
        let mut p = CetusPool::default();
        while let Some(field) = d.peek_field() {
            let lo = d.position();
            match field.name.as_str() {
                "id"                  => { d.skip_field()?; }
                "coin_a"              => p.coin_a = u64_field(d)?,
                "coin_b"              => p.coin_b = u64_field(d)?,
                "tick_spacing"        => p.tick_spacing = u32_field(d)?,
                "fee_rate"            => p.fee_rate = u64_field(d)?,
                "liquidity"           => p.liquidity = u128_field(d)?,
                "current_sqrt_price"  => p.sqrt_price = u128_field(d)?,
                "current_tick_index"  => p.tick_index = I32::from(d.as_struct(
                                            &mut I32Visitor, /* … */)?),
                "tick_manager"        => p.ticks = d.as_struct(&mut TickManagerVisitor, ..)?,
                "is_pause"            => p.is_paused = bool_field(d)?,
                _                     => { d.skip_field()?; }
            }
            // byte range of the field just consumed
            trace!(field = %field.name, bytes = &d.bytes()[lo..d.position()]);
        }
        Ok(p)
    }
    move_core_types::visitor_default! { <'b, 'l> u8, u16, u32, u64, u128, u256, bool, address,
                                        signer, vector, variant = Err(DecodeError::NotAStruct) }
}
```

This buys, for free:

* **No intermediate allocation** — the only allocations are the typed struct's `Vec`s.
* **Byte offsets** — `driver.start()` / `driver.position()` give the exact BCS range of every value, which
  is what makes the dump in §5.6 self-verifying.
* **Forward compatibility** — an appended or reordered field is silently ignored; only a *missing* field we
  claim is an error (`DecodeError::MissingField`), and that is exactly the signal the state manager wants
  (§8.3).
* **Depth/size safety** — the visitor can carry a budget like `BoundedVisitor` does.

The same crate provides `birdai_move::Dump`, a generic `Visitor` that renders a field-by-field annotated
tree with byte offsets. That is `BoundedVisitor`'s job in Sui; the difference is that ours also records
offsets and renders `OptionU64`-style protocol structs without special cases — and it is ~80 lines.

### 5.4 Path projection for the hot path

For pricing we do not need the whole pool, only ~6 leaves. `annotated_extractor` already implements exactly
this: a path-guided visitor that skips everything else and only delegates the selected sub-value.

```rust
use move_core_types::annotated_extractor as AE;
let path = vec![AE::Element::Field("current_sqrt_price")];
let Some(sqrt_price) =
    AE::Extractor::deserialize_struct(bytes, layout.as_struct()?, &mut U128Leaf, path)?
else { return Err(DecodeError::MissingField("current_sqrt_price")) };
```

rev 1 proposed hand-rolling a "compiled `FieldPath` with precomputed byte offsets" for this. That was
reinventing `Extractor`. We now reuse `Extractor` and only add a **path cache** (`ahash` map from
`(StructTag, &'static str)` to a `Vec<Element>`), which is where the real repeated cost was.

### 5.5 The tick child

`tick_manager.ticks` is a **skip list inlined in the pool**, so its nodes are dynamic fields on the skip
list's *inner UID* `tick_manager.ticks.id` — **not** on the pool object. This is the single most common
place to get lost:

* `object(address: POOL) { dynamicFields }` returns **empty** (verified on mainnet).
* The nodes are reachable via `DynamicFieldKey { parent: <inner UID>, … }` — verified with GraphQL
  `address(address: <innerUid>) { dynamicFields }`, which returned
  `MoveValue` of type `…::skip_list::Node<…::tick::Tick>` keyed by `u64`.
* gRPC `Client::get_dynamic_fields(parent = inner_uid, …)` reads the same index and is the primary path;
  the committed fixtures pin the round trip: their tick children are reachable through the inner
  UID's owner index, which is exactly what the live pagination returns.

Node key semantics (derived and verified):

```
score = tick_index + 443_636                       // Cetus MAX_TICK bias → orderable u64
node 509_466  ⇒  index.bits = 65_830   and   65_830 + 443_636 = 509_466   ✓
```

so `score` is not an opaque handle: `tick_index = score − 443636`, and a tick query is a skip-list
walk (16 levels, `nexts: vector<OptionU64>`) from `head`, giving O(log n) lookup —
implemented in `birdai-tick`. `Follow`/`Locate` used by the CLI:

* `Locate::Nearest(abs)` — bracketing initialised ticks around a target index (used by `decode` and by the
  "one step, no crossing" assertion).
* `Locate::Score(t)` — exact node for a tick, via the walk.

`birdai-tick` also validates the index against the skip list itself: `head[0]` gives the first node and
`size` must equal the number of reachable nodes; a mismatch is a hard error, because a drifted tick index
silently misprices every quote.

### 5.6 Required special handling, documented per the task

| Case | Reality in these objects | Handling |
|---|---|---|
| **Generics** | `Pool<USDC, SUI>` — both parameters are `phantom`, so they occupy **zero BCS bytes**. The layout of `Pool<X, Y>` is byte-identical for every `X, Y`; only the tag differs. | Resolve the **fully instantiated** `StructTag`; cache key is the instantiated tag. Never key a cache on `Pool` alone. |
| **`Balance<T>`** | `struct Balance<phantom T> { value: u64 }` — inlined, **not** a child object. | Decoded one level down as `u64`; `Extractor` path `["coin_a","value"]`. |
| **std `Option<T>`** | `struct Option<T> { vec: vector<T> }` → BCS `ULEB(len∈{0,1}) [+ T]`. Appears as `Option<ID>` in `LinkedTable::head/tail`. | Native `MoveTypeLayout::Vector` handling; `OptionVisitor` in `sui-types` is the reference. |
| **Cetus `OptionU64`** | `struct OptionU64 { is_none: bool, v: u64 }` — a **custom** option, 9 bytes, both fields always present. | Never pattern-match "option" by name. The layout says `struct`, so a layout-driven visitor cannot get this wrong — and a name-driven decoder *always* will. This is the sharpest concrete argument for layout-driven decoding in the whole exercise. |
| **`Table` / `Bag` / `LinkedTable` / `SkipList`** | The parent BCS carries only `{ id: UID, size: u64 }`. | Treat `size` as an assertion against the child index, never as contents. Children are separate dynamic fields keyed by the inner UID. |
| **`I32` / `I128`** | `{ bits: u32 }` / `{ bits: u128 }`, two's complement. | `birdai-move` newtypes: decode as unsigned, reinterpret as signed. Verified: one `liquidity_net` reads `340282366920938463463374605480910926342`, i.e. a large **negative** value. |
| **`String` / `ascii::String` / `TypeName`** | `{ bytes: vector<u8> }`, ULEB length prefix, no null terminator. | Borrowed as `&[u8]`; `RpcVisitor` re-encodes byte vectors straight from the byte range without visiting elements — we do the same. |
| **Move enums (bytecode v6)** | Not used by A/B/C. | `MoveTypeLayout::Enum` / `MoveEnumLayout` / `VariantDriver` are supported by the reused visitor framework; `birdai_move::Dump` renders `@variant`. |
| **`UID` / `ID`** | `UID { id: ID { bytes: address } }` — two nested structs for one address. | Resolved automatically; the dump collapses it to `UID(0x…)` for readability with an offset note. |

### 5.7 Output

`cargo run -- decode --all` prints, for each object and for the tick child:

```
0x51e883ba…::pool::Pool<USDC,SUI>  @v995150484  (770 B)
  [  32.. 40] coin_a.value                      :- 325996934562
  [  40.. 48] coin_b.value                      :- 458377448572119
  [  48.. 52] tick_spacing                      :- 10
  [  52.. 60] fee_rate                          :- 500
  [  60.. 76] liquidity                         :- 120115891674982
  [  76.. 92] current_sqrt_price                :- 647308812393509050120  (Q64.64 → 35.0906810333)
  [  92.. 96] current_tick_index.bits           :- 71162   (I32 → 71162)
  …
  tick_manager.ticks.id                        ⇒ 0x7f07284d…  (inner UID; 648 dynamic-field children at capture)
```

Every offset is taken from `ValueDriver::{start, position}` and is asserted in tests against
`bcs::to_bytes` round-trips.

The "do not use the node's pre-parsed JSON as your decoder, it is fine for checking your answers"
requirement is satisfied structurally rather than by claim: no JSON path exists anywhere in the
decode stack — every command decodes BCS bytes against a resolved layout, and the node's JSON was
only ever compared by eye during research.

---

## 6. Task 2 — Classify

### 6.1 The test

Field names and "it holds balances" are explicitly disqualified. The test is **structural and
behavioural**, and every clause is mechanically decidable:

> **Price-discovery test.** An object `O` of type `T` is a trading venue with on-chain price discovery iff
> all three hold.

**(1) Inter-asset swap entry (static, from bytecode).**
Let `P` be the package that defines `T`. Some module of `P` must expose a function
`f` with `f.visibility == Public` (or `f.is_entry == true`) such that, for two **distinct indices**
`i ≠ j` into the venue's type parameters:

* some parameter is `OpenSignature { ref_: Some(Mutable), body: Datatype(D, args) }` where `D`'s
  arguments mention a type parameter — a `&mut` borrow of generic state;
* some parameter is `Datatype(0x2::coin::Coin, [TypeParameter(i)])` — a coin taken **in** (`Balance`
  parameters do not count: a caller-supplied deposit is not an exact-input leg);
* some return is `Datatype(0x2::coin::Coin | 0x2::balance::Balance, [TypeParameter(j)])` with
  `j ≠ i` — a different asset coming **out**.

The scan covers the **whole defining package**, not just the module that defines `T`: Cetus's
`pool` module has no swap at all — its 77 functions were scanned and none has the inter-asset
shape — so a module-scoped probe reports a false negative on the clearest venue in the set.
Separately, the `package::module::function` the chain actually executed is resolved and tested
with a looser shape (legs on two type parameters, direction carried by a `bool` rather than the
types, as in `pool_script_v2::swap_b2a`), because the signature of the function that really ran
is the strongest available evidence.

Read directly off `FunctionDef { visibility, is_entry, type_params, parameters: Vec<OpenSignature>, return_ }`
(`sui-package-resolver/src/lib.rs:227-243`), with `OpenSignatureBody::Datatype(DatatypeKey, Vec<OpenSignatureBody>)`
and `OpenSignatureBody::TypeParameter(u16)`. Coin and balance are matched by address (`0x2`) as
well as by name. No field-name heuristics, no registry, no ABI-string parsing.

**(2) Endogenous price state (empirical, from two versions).** `O` carries a numeric state variable (or
tuple) whose value is a pure function of `O`'s own fields, and that variable **moves monotonically with net
trade flow**: given two versions of `O` bracketing a transaction that used the entry from (1), the price
variable moved in the direction implied by the flow, and **no oracle object appears among that
transaction's input objects**.

**(3) No-oracle probe (static).** Neither `T`'s field types nor the parameter types of the entry found in
(1) reference an external price feed: we scan `Module::bytecode()`'s immediate `module_handles` and the
`DataDef` field types for a configured deny-set of oracle packages/modules (`pyth`, `supra`, `switchboard`,
and each vendor's local `oracle`/`price_oracle` module).

(1) and (3) are static and cheap; (2) is what separates "has a price field" from "discovers a price".

### 6.2 Verdicts (README text)

**Object A — `0x1eabed72…::pool::Pool<USDC, SUI>` (Cetus CLMM): yes.** The pool's own state carries
`liquidity`, `current_sqrt_price` (Q64.64) and `current_tick_index`, plus a skip list of initialised ticks
with `liquidity_net`; the marginal price is `(sqrt_price/2^64)²` adjusted by the active liquidity, a pure
function of the object's own fields. Probe (1) is satisfied by the entry the chain executed,
`pool_script_v2::swap_b2a`: `(&GlobalConfig, &mut Pool<T0, T1>, Coin<T0>, Coin<T1>, bool, …)` —
a mutable borrow of the pool's generic state with coin legs on both of its type parameters. Probe (2) holds concretely — transaction T moves `current_sqrt_price` from
`647_308_812_393_509_050_120` to `647_324_162_169_833_037_484` while `coin_a` falls and `coin_b` rises,
with no price feed among the inputs. Every unit of price in this object is discovered by trading against
it. **Passes all three probes.**

**Object B — `0x549e8b69…::native_pool::NativePool` (Volo liquid staking): no.** It is pool-shaped —
`pending: Balance<SUI>`, `collectable_fee: Balance<SUI>`, a `vaults` table, a `validators` map — and it
does hold SUI, but the SUI↔VSUI exchange rate is an **accounting ratio**: total staked principal plus
accrued rewards over total shares, moved by reward accrual and validator-set rebalancing, not by order
flow. Probe (1) fails: the module exposes `mint`/`redeem` against that ratio, not a function that exchanges
two held assets for each other at a quoted marginal price. Probe (2) fails: there is no price variable that
responds to a trade. Two `Balance` fields and a ratio is a vault, not a venue. **Fails (1) and (2).**

**Object C — `0xd899cf7d…::storage::Storage` (Navi lending): no.** It holds 35 reserves
(`Table<u8, ReserveData>`) and ~999k `UserInfo` entries, and borrowing and liquidation happen through it,
but asset prices inside it are **not discovered by trading within the object**: supplies, borrows and
interest follow a utilisation curve, and valuation and liquidation thresholds come from an **external
oracle**. Probe (1) fails: there is no function exchanging reserve X for reserve Y at a price derived from
`Storage`'s own state — only `deposit`/`withdraw`/`borrow`/`repay`/`liquidate`, which move one asset
against a share claim. Probe (2) fails. Probe (3) trips: oracle dependency. **Fails all three.**

### 6.3 Implementation

```rust
pub struct Classifier<L: LayoutSource> { layouts: Arc<L>, oracle_deny: OracleDenySet }

impl<L: LayoutSource> Classifier<L> {
    pub fn static_probe(&self, tag: &StructTag, layout: &MoveTypeLayout) -> Result<StaticEvidence>;
    pub fn price_state_probe(&self, before: &Object, after: &Object, tx: &CheckpointTransaction)
        -> Result<DynamicEvidence>;
    pub fn verdict(&self, ev: &[Evidence]) -> Verdict;   // is_venue + per-probe evidence
}
```

The CLI prints the matching `FunctionDef` (name, visibility, entry-ness, parameter and return signatures)
and the before/after price values, so the README's paragraphs are **backed by printed evidence**, not
assertion. `Evidence` is a plain enum, and `Verdict` carries `is_venue: bool` plus the reason per probe.

---

## 7. Task 3 — Recreate state and reproduce T

### 7.1 Pre-state

T is in checkpoint `320577815`; the version T read is
`object(A, atCheckpoint: 320577814)` = **995150484**. `ObjectSource::object_at_checkpoint` expresses this
directly. On the checkpoint path the same object arrives as `CheckpointTransaction::input_objects`, and
`effects.object_changes()` gives `input_version = 995150484` — both routes are asserted equal in a test.

### 7.2 The math, exactly

Direction is **B → A** (sell SUI, buy USDC). With `x` = virtual USDC, `y` = virtual SUI, `L = √(xy)`,
`sqrtP = √(y/x)` scaled by 2^64:

```
fee                 = ⌊amount_in · fee_rate / 1_000_000⌋            // floor, protocol constant
amount_in_after_fee = amount_in − fee
ΔS                  = ⌊amount_in_after_fee · 2^64 / L⌋             // floor
S'                  = S + ΔS
out                 = ⌊ ((L ≪ 64) · ΔS) / (S · S') ⌋               // floor — pool-favourable
```

Exact-in with a cross-tick loop generalises to:

```
while remaining > 0 {
    let target = next_initialised_sqrt_price_in_direction;   // from birdai-tick
    let (used_in, out_chunk) = step(S, target);
    ...
    if S' == target { cross_tick(target); } else { break; }
}
```

and the **crossing branch is asserted dead for T**: `S' < sqrt_price(71190)` (and a fortiori
below the research-time `sqrt_price(71180)`), so no tick is crossed and
the single-step result is the final result.

### 7.3 Why `U256` is sufficient (rev 1 got this wrong)

rev 1 claimed 512-bit intermediates. They are not needed, and the bound is provable:

* Each `Balance<T>` holds a `u64`, so virtual reserves are bounded by total supply: `x_v, y_v < 2^64`.
* `L = √(x_v · y_v)` for an in-range pool ⇒ **`L < 2^64`**.
* Therefore `L ≪ 64 < 2^128`, and with `ΔS < 2^128`, the numerator `(L ≪ 64) · ΔS < 2^256` — exactly
  representable in `U256`.
* The denominator `S · S'`: `S, S' < 2^128` ⇒ `S · S' < 2^256` — also exactly representable.
* `ΔS = ⌊amount_in · 2^64 / L⌋`: `amount_in < 2^64` ⇒ the numerator is `< 2^128`.

So every intermediate fits in `move_core_types::u256::U256`, whose `checked_mul` is a full 256×256
multiplication and whose `checked_div` is exact. The final downcast uses `TryFrom<U256> for u128`
(lossless, returns `U256CastError`) rather than `unchecked_as_u128`.

**Guard rail.** `U256`'s `Add`/`Sub`/`Mul`/`Div` operator impls are **wrapping** (documented "Ignores
overflows"), and `Div` panics on a zero divisor. `birdai-amm` therefore defines
`struct CheckedU256(U256)` exposing only `checked_*` operations and a `TryFrom<u128>`/`Into<u128>` pair, and
a `clippy::disallowed_methods` entry forbids operator use on `U256` inside the crate. Arithmetic failures
surface as `AmmError::{Overflow, DivByZero, ZeroLiquidity}`, never as a silent wrap.

`L == 0` and `S == 0` are rejected explicitly (`ZeroLiquidity` / `InvalidSqrtPrice`) rather than aborting
for the same reason the Move code would.

### 7.4 Rounding and the comparison with the chain

**The recomputation equals the chain exactly: 81 168 759, Δ = 0.** The rounding directions that produce
that agreement, and that the code enforces:

| Step | Direction | Rationale |
|---|---|---|
| Fee | floor | Protocol constant; `fee_rate` is per `1e6` (verified: `500/1e6 × 100e9 = 50e6`) |
| `ΔS` | floor | Smallest price improvement consistent with the input — pool-favourable |
| Output `out` | floor | Pool-favourable; matches Uniswap-V3-lineage `getAmount0Delta(round_up = false)` |
| Input leg, exact-out mode | ceil | Pool-favourable (not exercised by T) |

**Robustness, stated explicitly in the README.** Recomputing with `ΔS` rounded **up** also yields
81 168 759: the step is only ~0.47 tick wide, so a one-unit change in `ΔS` perturbs the output by far less
than one base unit. The reproduction is exact *and* insensitive, which is stronger than a lucky match. A
`f64` path would give 81 168 759.00 ± 1 with an unreliable last digit — which is why the implementation is
integer-only and `f64` appears only in `proptest` oracles and display code (`calibrate` prints
`S/2^64` and `1.0001^(t/2)` side by side to re-derive Q64).

The reproduction is a **test**, not a printout:
`assert_eq!(reproduce(pre_state, &tx)?, 81_168_759)`.

---

## 8. Task 4 — Keeping state current from the checkpoint stream

### 8.1 Transport: reuse the framework, own the state

`sui_indexer_alt_framework::ingestion` is already a complete, DB-free checkpoint pipeline: hybrid
gRPC-streaming + object-store backfill, chain-id checks, exponential backoff, adaptive concurrency,
per-subscriber backpressure, per-item timeouts. `pipeline::*` is not usable (it requires a `Store`), so we
take the ingestion half and write our own state layer:

```rust
let mut svc = IngestionService::new(
    ClientArgs {
        ingestion: IngestionClientArgs { rpc_api_url: Some(rpc_url), ..Default::default() },
        streaming: StreamingClientArgs { streaming_url: Some(grpc_uri) },
    },
    IngestConfig { ingest_concurrency: ConcurrencyConfig::default(), ..Default::default() },
    Some("birdai"), &registry,
)?;
let mut rx = svc.subscribe_bounded(8);
let service = svc.run(start_cp..).await?;          // sui_futures::service::Service
while let Some(env) = rx.recv().await {            // Arc<CheckpointEnvelope>
    manager.apply(&env.checkpoint)?;               // Arc<Checkpoint>, chain_id
}
```

`CheckpointEnvelope { checkpoint: Arc<Checkpoint>, chain_id }`, and `Checkpoint` is the modern
deduplicated shape: `{ summary, contents, transactions: Vec<ExecutedTransaction>, object_set: ObjectSet }`
with `ExecutedTransaction::{input_objects, output_objects, created_objects}` resolved against the
`ObjectSet` via `effects.object_changes()`.

### 8.2 The write path

```
&Checkpoint
 └─ package observations                       → layouts.note_packages(versions)
 └─ for each ExecutedTransaction, in order
      └─ effects.object_changes()              → for each change {id, in_v, out_v}:
           ├─ out_v.is_none()  ⇒ tombstone(id) + drop its child entries
           └─ else ⇒ out = object_set[(id, out_v)]  (missing ⇒ counted failure, not fatal)
                  ├─ venue-shaped tag ⇒ candidate for decode
                  └─ dynamic-field tag ⇒ (parent, field_id) for the child index
 └─ distinct tags resolved once                → layouts.layout(tag)
 └─ BCS copied out, decoded on the blocking pool (rayon via spawn_blocking)
 └─ publish per object (version-monotone; equal is a retried-checkpoint no-op)
 └─ children indexed only for tracked pools' inner UIDs (bounded per parent)
 └─ checkpoints += 1; last_applied = sequence   (duplicate delivery skips whole)
```

* **No atomic snapshot — and that is documented, not hidden.** Each slot is an immutable
  `Arc<VenueSlot>` in an `scc::HashMap`, replaced per object; the checkpoint counter moves last
  as a monotone watermark, not a barrier. A reader that arrives mid-checkpoint can see a mix of
  old and new slots. A single slot is always consistent; two slots are not guaranteed to be from
  the same checkpoint, and pricing code is written to that contract.
* **Parallel decode without stalling the stream**: the BCS is copied out of the checkpoint and
  decoded with `rayon` on the blocking pool (`tokio::task::spawn_blocking`), because the layouts
  are already in hand and an `async` worker must not sit on CPU-bound work. The Sui crates are
  `Sync` where it matters, and `Resolver::type_layout` is `async`, so distinct tags are
  pre-resolved in one pass, then decoding is synchronous.
* **Lock-free reads**: `scc::HashMap<ObjectID, Arc<VenueSlot>>` plus a second map for children;
  atomics carry the counters. A pricing thread never blocks a writer.
* **Duplicates and regressions**: a checkpoint at or below `last_applied` is a duplicate delivery
  and skips whole; within a checkpoint, an incoming version older than the slot's is
  `OutOfOrder` (an error — it would silently serve a stale price), while the same version is a
  no-op. This is what makes re-applying a checkpoint that touched one object several times safe.

### 8.3 The three hard problems

**New pools.** Venue identity starts from **`module::name`** (see `venue_kind_of`), not from an
allow-list, so a pool deployed under a new package is picked up on first sight. The name is only
a hint: the resolved layout's field set is the test (`layout_has_shape`), and a same-named struct
with a different layout is counted as `unrecognised`, not `failed` — several mainnet packages
define their own `pool::Pool`.

**Dynamic-field churn.** Children are separate objects whose owner is an **inner UID**, so they do not
look like children at all in the change set. The manager routes them by
`Owner::ObjectOwner(parent)` and indexes **only the inner UIDs of tracked Cetus pools** — a
lending ledger's ~999k user entries are never indexed at all — with each parent's set bounded by
a capacity whose drops are counted and warned, not silent. A deleted parent's entries die with it.
Tick nodes are dense enough (hundreds) to index fully, which is what makes tick-range pricing
O(log n).

The skip list's declared `size` is used **only as a consistency signal**: `Ticks::new` (same-state
reads) asserts it, while `Ticks::from_children` (children read at the present against historical
metadata) downgrades a mismatch to a reported `SizeSkew`, because dynamic fields can only be
listed as of now. A mismatch is reported rather than hidden, because a drifted child index is the
failure mode that silently misprices everything downstream.

**Package upgrades that change layouts.** This is the gap `PackageStoreWithLruCache` does not close: it
caches `Package` by storage id and re-fetches on demand, but it has **no invalidation hook**, so a cached
`MoveTypeLayout` for a dependent type survives a package upgrade. `birdai-resolve` adds:

* cache keyed by **canonical `StructTag`**, with every cached layout carrying the `(package, version)`
  pairs it was resolved against; a read re-checks those versions and treats the entry as stale the
  moment one moved;
* a `LayoutRegistry::note_packages(versions)` hook driven by `MovePackage` objects observed in the
  change set, which evicts every cached layout whose dependencies moved — **transitively**, since pool
  A depends on three packages (`pool`, `i32`, `skip_list`) and an upgrade to any of them changes the
  pool's instantiated layout. Invalidation is push-only: versions arrive through this hook, so a
  poller that never feeds checkpoints must call it from its own observations;
* a **fingerprint** (`blake3` over the compiled layout, including field names and tags) stored next to each
  published state, so tooling can assert a replay decoded with the exact layout that was live at that checkpoint
  (the manager warns when a tracked venue's fingerprint changes under it).

Typed states are built by **field name** (§5.3), so an appended or reordered field needs no code change;
a renamed or removed field surfaces as `DecodeError::MissingField`, which is routed to an alert instead of
silently producing a stale price. That is the deliberate boundary: *layout drift is loud, not silent.*

### 8.4 The boundary, and what changes inside a validator

The boundary is exactly three things — **bytes, tag, version** — entering a typed state:

```
raw object (BCS bytes + StructTag + SequenceNumber + Owner)
   → LayoutSource::layout(tag)                       [layout, cached, upgrade-aware]
   → Venue::decode(bytes, layout, ctx)               [one pass, name-matched, no tree]
   → VenueSlot                                       [published per object, watermarked per checkpoint]
```

| | Checkpoint stream | Inside a validator |
|---|---|---|
| Raw objects | `sui_types::object::Object` from `Checkpoint::object_set` | the same type from the object store / `InputObjects` |
| Version truth | Final and ordered; a checkpoint commits atomically | **Provisional**: state changes before consensus, so the manager needs `begin_tx / apply / commit_or_abort` with rollback on re-execution, and must not publish a checkpoint cursor until the effects are durable |
| Layouts | `SourcePackageStore` over gRPC; async; upgrade-aware cached | straight out of the validator's `ModuleCache`; **synchronous**, and a package upgrade is visible the instant the publish executes |
| BCS provenance | From the wire; validated by the checkpoint digest | The very buffer the VM executed against — already trusted, so decode can skip validation and borrow directly, no copy |
| Children | Must be re-associated by owner/effects | The VM hands over the child objects it loaded, so `ChildIndex` can be built from `InputObjects` directly |
| Failure mode | Lag or a gap → resync from a checkpoint | Re-org or aborted execution → MVCC rollback |

Concretely, switching source means: implement `ObjectSource` for the validator's object store
(the gRPC impl is already the same shape), serve layouts from its `ModuleCache` behind the same
`LayoutSource` trait (or keep `async` with a ready future), and add the two-phase commit to
`StateManager`. **No venue, math, tick or decode code changes.** That is the payoff of putting
the boundary at bytes+tag+version rather than at "an HTTP client".

### 8.5 Ordering hazard, as implemented

A checkpoint's `object_set` can contain several versions of the same object, and
`effects.object_changes()` is per transaction — the captured checkpoint mutates pool A twice.
Two guards share the work:

* checkpoints are deduplicated by sequence (`last_applied`): a checkpoint at or below the last
  applied one is a duplicate delivery and skips whole, which is what makes re-applying safe;
* versions are monotone per object (`should_publish`): an incoming version older than the slot's
  is `OutOfOrder` — an error, not a silent last-writer-wins — while the same version is a no-op.

The first catches retries; the second catches out-of-order application *and* the "same object
mutated twice in one checkpoint" case. Both are pinned by tests that replay the captured
checkpoint offline.

---

## 9. Review of rev 1 — twelve defects, and what replaced them

The re-review was done against the actual sources, not against memory. Each defect below is a real change
in this revision.

| # | rev 1 | Defect | rev 2 |
|---|---|---|---|
| 1 | Zero Sui dependencies; hand-written BCS reader, layout IR, cursor, projection | Reimplemented ~4 crates' worth of already-better code. `annotated_visitor` gives byte offsets, budgets, enums, variant tags, and auto-skipping; `annotated_extractor` gives path projection; `BoundedVisitor` gives annotated dumps; `Resolver` gives layout resolution from real package bytecode with canonicalisation. Hand-rolling all that adds bugs and removes nothing | Sui crates pinned by git rev; own only the six crates in §3.2 |
| 2 | Compiled `FieldPath` → byte offsets, as the performance story | Reinventing `annotated_extractor::Extractor` | Reuse `Extractor`; cache the `Vec<Element>` path, which was the only real repeated cost |
| 3 | "Intermediates need `U512`" | **Provably false.** `L < 2^64` ⇒ numerator `< 2^256`; `S·S' < 2^256` | `U256` throughout, with the bound proved and a `CheckedU256` wrapper, because `U256`'s operators wrap |
| 4 | `MoveStructLayout::{Runtime, WithFields, WithTypes}`; `MoveValue::to_annotated_value` | None of these exist in this revision | `annotated_value::MoveStructLayout { type_, fields }`; `MoveValue::decorate` / `visit_deserialize` |
| 5 | `sui-graphql-rpc-client` | That crate does not exist | gRPC v2 `sui_rpc_api::Client`; GraphQL only as cross-check |
| 6 | GraphQL as the primary fetch path | gRPC returns a native `sui_types::object::Object` (the checkpoint type), supports read masks, batching and dynamic-field paging, and is already a `PackageStore` | gRPC primary; GraphQL for `--check` and the inner-UID fallback |
| 7 | Hand-rolled `CheckpointSource` over `sui-data-ingestion-core` | The framework's `ingestion` module already does hybrid streaming + backfill, retries, backoff, backpressure, chain-id checks — and is DB-free with `default-features = false` | `IngestionService` + our `StateManager`; `default-features = false` to drop Diesel/Postgres |
| 8 | Classifier probes described informally ("the package exposes a swap entry") | Not decidable, and the task explicitly forbids field-name reasoning | Probes specified to `FunctionDef` / `OpenSignatureBody::{Datatype, TypeParameter}` level (§6.1) |
| 9 | `ChildIndex` keyed vaguely by "parent + key" | Children of inner UIDs are not discoverable by owner id alone | `derive_dynamic_field_id` + `FieldVisitor`'s zero-copy `Field { name_bytes, value_bytes }`, three arrival paths |
| 10 | Layout cache keyed by `StructTag` | Survives package upgrades — a correctness bug, and the exact thing task 4 asks about | Key `(canonical tag, defining package version)` + transitive invalidation + `blake3` fingerprint |
| 11 | 4-hour plan, with "ship half a tick loop" as the fallback | Produces a half-finished deliverable to protect an artificial deadline | Quality-first milestones (§11); the multi-tick loop is in scope |
| 12 | Tests omitted byte-range invariants | The visitor framework makes `start()`/`position()` observable, so offset correctness is testable | Offset assertions, round-trip tests, `skip`/`project` agreement tests (§10) |

Two things survived review unchanged and are load-bearing: the **raw/typed seam** (`bytes + tag + version`
in, typed state out) and the **quality of the on-chain research** (§1) — including the Q64.64 derivation,
`score = tick + 443636`, the inner-UID dynamic-field trap, and the exact reproduction.

---

## 10. Testing

117 tests, all offline against the committed fixture set (`cargo test` never touches the network):

* **Golden fixtures** — A, B, C, the tick children and checkpoint `320577815`'s kept transactions:
  bytes + layout + expected field dump with offsets.
* **Reproduction as a test** — `assert_eq!(quote, 81_168_759)` in `birdai-amm`, and the same number
  asserted end to end by `reproduce --fixtures fixtures`.
* **State replay as tests** — checkpoint 320 577 815 applies offline through `StateManager`: pool A
  is tracked at its output version, re-applying is a no-op, and versions are monotone per object.
* **Offset invariants** — for every leaf, `&bytes[start..position]` re-decoded standalone equals the leaf
  value; `skip` paths keep parent spans valid under depth/item caps.
* **`proptest`** in `birdai-amm`: round-trip `sqrt_price_at_tick`/`tick_at_sqrt_price` over the whole
  grid; monotonicity sampled across it.
* **Boundary cases** — empty vector, `OptionU64 { is_none: true }` with a stale `v` (payload kept),
  missing `v` (hard `MissingField`), `L = 0`, `S = 0`, `u64::MAX` balances, single-element skip list,
  ticks outside `[MIN_TICK, MAX_TICK]`, wrong-side price limits, stale boundaries, fee rates at and
  above the denominator.
* **Classifier tests** — the deny set (case, fragments, addresses), `referenced_types` descending into
  enum variants, and the `is_endogenous` truth table (flow direction × price move × oracle veto).
* **Tick index tests** — score/key consistency, duplicate and dangling links, backwards links
  (`Unordered`), price-deviation buckets, bracketing and tie-breaking.
* `just format && just lint && just test && just mutation` (cargo-mutants); mutation score is reported
  in the README's Verification section.

---

## 11. Milestones (quality-first)

No deadline. Each milestone is independently useful and leaves the repo green.

**M0 — Skeleton and pins.** Workspace with the pinned git deps; `rust-toolchain.toml` 1.96.1;
`just deps-check`; `ObjectSource`/`LayoutSource` traits with a gRPC implementation; fixture capture tool.
*Exit:* `cargo run -- fetch --out fixtures` writes the three objects, their layouts and checkpoint 320577815 to
`fixtures/`.

**M1 — Decode.** `birdai-move`: `I32`/`I128`/`OptionU64`, `Dump` visitor, `StructDecoder` helpers.
`birdai-tick`: skip-list indexing and `Locate`. Venue visitors for A/B/C.
*Exit:* `decode` prints offset-annotated dumps; the committed fixtures pin the inner-UID round trip.

**M2 — Classify.** `birdai-venue::Classifier` with the three probes, the oracle deny-set, and evidence
rendering.
*Exit:* `classify` prints per-probe evidence and the verdicts in §6.2.

**M3 — Recreate.** `birdai-amm`: `CheckedU256`, `tick_math` (`sqrt_price_at_tick`, exactly as
`1.0001^(t/2)` with the bit-decomposition trick), `sqrt_price_math`, `swap_math`, and the multi-tick loop.
*Exit:* `reproduce` prints L, S, tick range, the step, the output, and `Δ`; the golden test is 81 168 759;
proptest suite green.

**M4 — State.** `birdai-state`: `StateManager`, per-parent-bounded child index, `LayoutRegistry` with transitive
invalidation, per-object `scc` slots with checkpoint watermarks and version-monotone publication.
*Exit:* `follow --from 320577815 --count 5` tracks venues live and loads a new pool's ticks on first
sight; package upgrade invalidation covered by a synthetic test; the captured checkpoint replays
offline through the same `apply_checkpoint`.

**M5 — Polish.** README (the graded §2/§3/§4 answers), `--fixtures DIR` replay mode, `just ci` green,
mutation score reported.

---

## 12. Risks

| Risk | Mitigation |
|---|---|
| Sui git dep build time / disk on a fresh clone | Pinned rev + `Cargo.lock` pinning; `kache` wrapper; `[profile.dev.package."*"] opt-level = 2`; README states the one-time network requirement |
| Public fullnode gRPC / GraphQL rate limits during a demo | Fixtures + `--fixtures DIR`; every command runs unchanged against the capture, so the demo needs no node at all |
| Public checkpoint object stores retain only ~30 days | Checkpoint `320577815` is fetched once and committed as a fixture |
| Cetus package upgraded between research and grading | `calibrate` re-derives the fixed-point format from `S` and `tick`; typed decode is name-based; the layout cache is version-keyed |
| `Resolver` limit defaults differ from the pool's nesting depth | Limits set explicitly and asserted; `Pool` is shallow (< 20 nodes) |
| `PackageStoreWithLruCache` re-fetch semantics under `--offline` | Fixture store implements `PackageStore` directly, so the resolver path is exercised end-to-end offline |
| Tick child not enumerated under `--offline` | All 653 tick nodes captured to fixtures, plus the venue layouts of every pool the checkpoint touches |

---

## 13. Open questions for the Birdai team

1. Is `current_sqrt_price` Q64.64 across **all** Cetus deployments and generations? We derive it per pool at
   runtime via `calibrate`, but a confirmation would let us assert instead of derive.
2. For the state manager: do you price from a snapshot at checkpoint N, or do you need **intra-checkpoint
   ordering** (transaction order within a checkpoint) for MEV? This design publishes per checkpoint and
   keeps per-transaction versioning available but unused.
3. Do you include Cetus **rewarder** accrual (`rewarder_manager`, `points_growth_global`) when pricing, or
   only the swap leg? The math is unaffected; the state surface is not.
4. Is the target deployment a follower process (checkpoint stream) or co-located with execution? §8.4
   supports both, but the answer changes where the effort goes (rollback semantics vs. resync).
5. For venue coverage: is a bytecode-level classifier (§6) enough, or do you maintain a curated registry
   with per-protocol adapters? Our `Venue` trait is designed to be the adapter; the classifier is designed
   to be the discovery mechanism.
6. Do you want tick-level state indexed for **every** pool (653 nodes for this one alone) or only for
   pools inside your quoting set? §8.3 bounds it either way, but it is a memory/coverage trade-off you
   already have an opinion about.

---

## 14. rev 3 — what building it changed

Every item below was found by running against mainnet, and each one contradicts something §1–§13
asserts or assumes. They are recorded rather than quietly patched, because the wrong version is the
plausible one.

| # | § | rev 2 said | Reality | Fix |
|---|---|---|---|---|
| 13 | §6.1 | The swap entry is in the module that defines `T`. | **Cetus's `pool` module has no swap.** All 77 of its functions were scanned; none has the inter-asset shape. The entry that moved pool A is `0xae9c208c…::pool_script_v2::swap_b2a`, a sibling package. | The probe scans the defining package **and** resolves the `package::module::function` the chain executed, accepting a looser shape for the latter because script-style entries express the direction in a `bool` rather than in the types. `EntryEvidence::{StaticScan, ObservedCall}` records which applied. |
| 14 | §8.3 | Venue identity is by `module::name`, so a new pool is picked up automatically. | Name alone is not identity: `follow` met 27 objects named `pool::Pool` from other packages with different layouts and failed to decode every one of them. | The name is a hint and the **resolved layout's field set** is the test (`layout_has_shape`). Collisions are counted as `unrecognised`, distinct from `failures`. |
| 15 | §5.5, §8.3 | Children are re-associated with parents and the declared `size` is asserted. | A pool's children **cannot be read at a historical version**. T consumed pool A at version 995 150 484, which declares 650 ticks; enumerating today returns 653. | `Ticks::from_children` downgrades the size check to a reported `SizeSkew` while keeping every other invariant, and callers must handle the skew. See item 17 for why the quote does not need the tick set at all. |
| 16 | §7.2, §7.4 | `sqrt_price_at_tick` is `⌊1.0001^(t/2)·2^64⌋` and tick nodes can be validated against it exactly. | Two of pool A's 654 nodes differ, by up to 7 units at `√P ≈ 7.9·10^28` (relative error under `2^-90`). Exhaustive search over Q128.128 with floor/round/ceil factor tables, truncating or ceiling the final narrowing, and an integer-square-root variant found **no** variant that reproduces every observed tick. | The on-chain values are authoritative; `sqrt_price_at_tick` is documented as approximate and validation uses a **relative** tolerance (`TICK_PRICE_TOLERANCE_BITS = 48`) with the deviation histogram reported. The swap math already used stored prices, so the reproduction was unaffected. |
| 17 | §6.5 | The single step is proven by comparing `S'` against the next initialised tick. | That comparison needs the tick set, which item 15 shows is unavailable at a historical version. | The primary argument is now `tick_spacing`: a price move smaller than the spacing cannot reach another initialised tick, because initialised ticks lie on the spacing grid. `reproduce` quotes both with and without boundaries and asserts they agree. |
| 18 | §2.2 | `default-features = false` on `sui-indexer-alt-framework` drops Diesel/Postgres. | It does not. `sui-indexer-alt-metrics` — a non-optional dependency of the framework — depends on `sui-pg-db` unconditionally, so Diesel, `diesel-async`, `diesel_migrations` and `tokio-postgres` are in the graph regardless. | The framework is still right, because nothing else ships hybrid streaming + backfill with retries and backpressure. The claim is corrected; the flag is kept because Sui's own root uses it. |
| 19 | §7.3 | The CLMM math needs care around `U256`'s semantics. | `U256`'s `Add`/`Sub`/`Mul` **wrap** and `Div`/`Rem` panic on a zero divisor; the `checked_*` variants exist but nothing forces their use. | Arithmetic goes through a `CheckedU256` newtype that exposes only checked operations and converts failures into `AmmError`; it is the only way the crate touches a 256-bit value. |
| 20 | §10 | Tests cover the golden numbers. | Three test *expectations* were wrong on first run: a rounding bound that ignored the magnitude of the truncated factors, a `nearest` tie point miscalculated, and a crossing case whose input was large enough to overflow. All three were fixed against measured data, not loosened. | The values that actually hold are recorded in the tests' comments. |
| 21 | §2.2 | Dependency pinning is a build detail. | `allocative 0.3.6` moved to `hashbrown 0.16` while the Move package system's `starlark_map 0.13.0` uses `0.14.5`, so the derived `Allocative` impls stopped matching and `starlark_map` failed to compile. Sui's lock holds `allocative 0.3.4`. | `Cargo.lock` pins `allocative` to `0.3.4` with the reason recorded (see also item 29 for the nightly patch). A lock-file drift is a build failure, not a warning. |
| 22 | §3.1 | `sui_rpc_resolver::RpcPackageStore` is the package-store backend. | It builds its own client from a URL, so it can carry neither an API key nor a fixture. | `SourcePackageStore<O>` implements `PackageStore` over any `ObjectSource` in fifteen lines. The resolver above it is untouched — which is precisely the point of `PackageStore` being a trait — and one source now serves objects, package bytecode and, if it ever exists, a validator's object store. |
| 23 | §7.1 | `ObjectSource::objects` can be a batched RPC call. | `Client::batch_get_objects` collapses a **single** missing object into a wholesale error, and a missing object is not exotic: enumerating a pool's 650 tick nodes and fetching them afterwards leaves a window in which a tick is removed. This took a capture down. | Batch first for round-trip efficiency, then retry object by object and skip anything that is gone. The retry is in the source, so every caller inherits it. |
| 24 | — | A fixture directory that does not exist can load as an empty set. | That turned a mistyped `--fixtures` path into "object not found" three commands later. | `Fixtures::load` requires the manifest and fails with `FixtureError::Missing` naming the directory. The committed-set tests skip explicitly on absence instead of relying on silent defaults. |
| 25 | — | Hosted providers' Sui endpoints serve gRPC v2. | `shared.eu-central-1.getblock.io/<key>` answered every request with `Missing token-id` — with the key in the URL path, and in `x-api-key`, `x-token-id` and `Authorization: Bearer` in turn. | `--api-key` is supported (sending the first two plus a bearer token) because providers that *do* offer gRPC expect a header; the capture used Sui's own endpoints. Worth proving the keyed path against a provider that enables it. |
| 26 | §4.1 | One endpoint is enough. | **Sui's two public mainnet endpoints are not interchangeable.** `fullnode.mainnet.sui.io` serves the whole API but keeps only a bounded window of checkpoints — `GetObject` succeeds while `GetCheckpoint` for checkpoint 320 577 815 returns transient `unavailable` or `NotFound`. `archive.mainnet.sui.io` keeps the full history but does **not** implement `StateService`: `ListDynamicFields` answers `Unimplemented`. | `GrpcObjectSource` holds a second client used for `Checkpoint` reads, with a fallback to the primary on archival failure, and `--archive-url` (default `archive.mainnet.sui.io`, `""` to opt out). The same split later covered versioned object reads too — the fullnode prunes those as well, so `object` routes `Some(version)` to the archive first and `None` to the fullnode first, each falling back to the other. This is the same shape the validator variant takes — one source, several transports behind it — so it cost a branch rather than a redesign. |
| 27 | §10 | Mutation testing is future work. | `cargo mutants -p birdai-amm -p birdai-tick` (248 mutants): 204 caught, 38 unviable, 6 missed — and the 6 split into 4 equivalent-by-construction plus 2 real gaps. The gaps were a self-linking skip-list node (it resolves in the score map, so only the `!= position` half of the resolvability check rejects it) and a negative price deviation whose magnitude needs a subtraction (division collapses every small negative deviation to −1). | Both gaps now have killer tests; a scoped re-run over `birdai-tick/src/index.rs` reports 103 caught, 33 unviable, **0 missed**. The 4 equivalents (`delta_a`/`delta_b` `||`→`&&`, min-clamp `<`→`==`/`<=`) are pinned by in-code comments plus tests that lock the equivalence. Effective kill rate on killable mutants: 100%. |
| 28 | §8.3 | A tick snapshot read over RPC is consistent. | It is not, on a live pool: listing the children is paginated and fetching them is a later round trip, so a tick added or removed in between leaves a snapshot whose link graph does not close. Online `reproduce` failed with `node 515776 links to 515836, which is not in the index` — the pool had grown new ticks since the fixtures were captured. Retrying the fetch alone cannot help, because the inconsistency is in the *listing*, not the fetch. | `load_tick_index` re-takes the whole snapshot once on any validation failure and only then fails loudly. Observed live: the retry fired (`node 508836 links to 508866`), the second snapshot validated, and the quote still matched the chain exactly. |
| 29 | §2.2 | `just lint` passes as written. | It did not, for three reasons, all outside our code: (a) `allocative <= 0.3.5` fails on recent nightlies (duplicate `Allocative` impls for `!` vs `Infallible`, E0119), which reds the nightly-clippy step and CI with it; (b) `cargo workspace-inheritance-check --check` was never a valid flag — the tool checks by default; (c) 23 declared dependencies were dead (template leftovers like `config`/`rustls`, and refs removed by refactors such as `sui-rpc-resolver` after item 22). | (a) `third-party/allocative`: vendored 0.3.4 with exactly the redundant `!` impl deleted (stable never compiled it, so behaviour is unchanged there) plus warning fixes, wired via `[patch.crates-io]` kept last in the root manifest — a `[patch.*]` header ends the preceding table, so it must never be spliced into `[workspace.dependencies]`. (b) The Justfile recipe now calls the tool bare. (c) All 23 removed after grep-verifying zero uses; `cargo shear` is clean. |
| 30 | §8.2 | One bad object fails the checkpoint, and re-applying is free. | Neither held. Replaying the captured checkpoint offline showed a second Cetus deployment (`0x91bfbc38…::pool::Pool` with three type parameters) whose package the capture predates — layout resolution failed and aborted the whole apply — and the same checkpoint mutates pool A twice, so a naive re-apply walks versions backwards. | Per-tag resolution failures skip their objects with a counted failure; checkpoints deduplicate by sequence (`last_applied`) and versions stay monotone per object (`should_publish`, with same-version re-apply a no-op). Both arms are pinned by offline replay tests. |
| 31 | §7.2 | Any `price_limit` is a valid bound. | A limit behind the price walked the price backwards through `next_price`, and a stale tick source could do the same through a boundary — while `UnreachablePriceLimit` sat unconstructed. `take_fee` also misused `DivByZero` for an invalid fee rate. | Up-front validation rejects a wrong-side limit and ignores a behind-price boundary (`is_ahead`); `take_fee` returns `InvalidFeeRate`. Each side has a test, including the stale-boundary quote matching the boundary-free one. |
| 32 | §5.3 | Decoders fail closed. | Two did not: `VecVisitor` pre-allocated from the untrusted BCS length prefix, and `OptionU64Decoder` defaulted a missing `v` to `0` — a `Some(0)` out of thin air on a pricing path. The visitor error conversion also discarded its source. | The reservation is capped (`MAX_VECTOR_PREALLOC`); a missing `v` is `MissingField`; the conversion keeps the message (`Annotation`). The `is_cetus_skip_list` module/name wildcard stays, but is now documented as a labelling hint with the upgrade trade-off stated. |
| 33 | §8.3, §11 | `fetch` captures what the commands decoded; `follow` tracks venues. | `fetch` never decoded the checkpoint's *other* venues, so their packages were missing offline; `follow` tracked new pools without ticks, so they could never be quoted (`install_ticks` had no caller). | `fetch` pre-resolves every venue-shaped object the recorded checkpoint carries; `follow` loads a new Cetus pool's ticks on first sight. The state manager itself only indexes children of tracked pools' inner UIDs, with per-parent caps whose drops are counted and warned. |

### Offline replay

§2.2's "fixtures are designed but not committed" is closed: `cargo run -- fetch --out fixtures`
captures 1.2 MB — pool A at three versions, B, C, 653 tick nodes, 12 packages, 9 layouts, and a
filtered checkpoint — and `cargo run -- --fixtures fixtures <command>` replays any of them offline.
The design constraint that made this cheap is that `ObjectSource` and `LayoutSource` were traits from
the start: switching to `FixtureObjectSource`/`FixtureLayoutSource` changed one constructor, and every
command works unchanged. `docs/design.md` §14 items 23–25 record the three bugs the replay exposed.

Two things §1 got right and that carried the whole exercise: the Q64.64 derivation from
`S / 1.0001^(tick/2)` — now the `calibrate` command — and the exact reproduction of transaction T
from `L`, `S` and the fee alone (`Δ = 0`, asserted as a test rather than printed).

---

## Appendix A — Working queries and endpoints

```
gRPC v2 : https://fullnode.mainnet.sui.io:443            (sui_rpc_api::Client)
GraphQL : https://graphql.mainnet.sui.io/graphql         (cross-check + inner-UID fallback)
blobs   : https://checkpoints.mainnet.sui.io/{n}.binpb.zst
```

```graphql
object(address: $a, version: $v)          # or atCheckpoint: $c   (UInt53) — there is NO objectAtVersion
{ version digest asMoveObject { contents { bcs type { repr layout } json } } }

address(address: $innerUid) {             # children of an INNER uid; object.dynamicFields returns []
  dynamicFields(first: 50, after: $cursor) {
    pageInfo { hasNextPage endCursor }
    nodes { name { type { repr } json bcs }
            value { __typename
                    ... on MoveValue  { bcs type { repr layout } }
                    ... on MoveObject { contents { bcs type { repr layout } } } } }
  }
}

transaction(digest: $d) { effects { checkpoint { sequenceNumber } } }   # `transaction`, not `transactionBlock`
```

`bcs` is `Base64` (standard alphabet, padded). `Object.digest` is Base58.

## Appendix B — Reference API notes

* `sui_package_resolver::Limits { max_type_argument_depth, max_type_argument_width, max_type_nodes,
  max_move_value_depth }` (`lib.rs:70-82`).
* `FunctionDef { visibility: Visibility, is_entry: bool, type_params: Vec<AbilitySet>,
  parameters: Vec<OpenSignature>, return_: Vec<OpenSignature> }` (`lib.rs:227-243`);
  `OpenSignature { ref_: Option<Reference>, body: OpenSignatureBody }`;
  `OpenSignatureBody::{Address,Bool,U8..U256,Vector(Box<_>),Datatype(DatatypeKey, Vec<_>),TypeParameter(u16)}`.
* `Module::{functions(after, before), function_def(name), bytecode(), structs/enums/datatypes}`.
* `move_core_types::u256::U256` — backed by `primitive_types::U256`; `checked_add/sub/mul/div/rem/shl/shr`;
  `TryFrom<U256> for u128`; **`Add`/`Sub`/`Mul` wrap**, `Div`/`Rem` panic on zero. No `as_u128`.
* `annotated_visitor::{Visitor, Traversal, NullTraversal, visitor_default!, ValueDriver, VecDriver,
  StructDriver, VariantDriver}`; drivers expose `start()`, `position()`, `bytes() -> &'b [u8]`,
  `remaining_bytes()`; the framework auto-drains unvisited fields/elements after a container returns.
* `annotated_extractor::{Extractor, Element}` — `Element::{Field(&str), Index(u64), Type(&TypeTag),
  Variant(&str)}`; `Extractor::deserialize_struct(bytes, layout, inner, path) -> Result<Option<V::Value>, _>`.
* `sui_types::object::bounded_visitor::BoundedVisitor` — annotated dump with a ~1 MiB name/type budget.
* `sui_types::dynamic_field::visitor::FieldVisitor` — zero-copy `Field { name_bytes, value_bytes }`.
* `sui_types::full_checkpoint_content::{CheckpointData, CheckpointTransaction, Checkpoint, ExecutedTransaction, ObjectSet}`.
* `sui_types::effects::TransactionEffectsAPI`; `ObjectChange { id, input_version, output_version, .. }`.
* `sui_indexer_alt_framework::ingestion` — `IngestionService`, `ClientArgs`, `IngestionConfig`,
  `CheckpointEnvelope`, `IngestionClientTrait`, `GrpcStreamingClient`, `CheckpointStream`;
  `default = ["cluster"]` pulls Postgres ⇒ always `default-features = false`.
