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
| Decode A/B/C + a tick child | `move_core_types::annotated_visitor` visitors + `annotated_extractor` + `sui_package_resolver::Resolver` | `cargo run -- decode --all` |
| Classify | Structural bytecode probe over `FunctionDef`/`OpenSignature` + empirical price-state probe | `cargo run -- classify --all` |
| Recreate T | `birdai-amm` integer CLMM math on the pre-state | `cargo run -- reproduce` → **81 168 759, matches chain** |
| Design note | `birdai-state` on `sui-indexer-alt-framework::ingestion` | `cargo run -- follow --from 320577815` + README §4 |

Pinned upstream: Sui mainnet branch `main` @ **`c8755d9c05209a22d91c07df76458992defd99c4`** (2026-09-09),
toolchain **1.96.1**, edition 2024. This is the tip of `main` as of writing (verified with
`git ls-remote`).

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

Latest state (version 996382523): `coin_a = 254_548_174_454`, `coin_b = 551_726_244_467_576`,
`liquidity = 68_693_527_635_052`, `current_sqrt_price = 673_624_336_522_390_633_733`,
`current_tick_index = 71959`.

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
| **71180** | 647_882_882_935_015_212_980 | **upper bound / next initialised tick** |
| 71190 | 648_206_889_171_250_166_865 | above |

Active range **[71060, 71180)**, active liquidity `L = 120_115_891_674_982`.

### 1.4 Reproduction

```
fee               = 100_000_000_000 × 500 / 1e6 = 50_000_000        (matches the stated fee)
amount_in_after_fee = 99_950_000_000
ΔS   = ⌊amount_in_after_fee · 2^64 / L⌋ = 15_349_776_323_987_364
S'   = S + ΔS = 647_324_162_169_833_037_484
out  = ⌊ (L ≪ 64) · ΔS / (S · S') ⌋ = 81_168_759     ← chain: 81_168_759, Δ = 0
```

`S' = 647_324_162_169_833_037_484 < sqrt_price(71180) = 647_882_882_935_015_212_980`
→ **no tick crossing; one step; `L` constant.** In tick units the move is ≈ 0.47 tick.

### 1.5 Objects B and C

**B — Volo `NativePool`** `0x549e8b69…::native_pool::NativePool`:
`id`, `pending { id, balance: Balance<SUI> }`, `collectable_fee { id, balance }`,
`validator_set { id, vaults: Table{ id, size: 5 }, validators: 0x2::vec_map::VecMap { contents: [(address, u64)] }, sorted_validators: vector<address>, … }`.

**C — Navi `Storage`** `0xd899cf7d…::storage::Storage` (208 bytes total):
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
SUI_REV = "c8755d9c05209a22d91c07df76458992defd99c4"
sui-types                 = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
sui-package-resolver      = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
sui-rpc-api               = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
sui-rpc-resolver          = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
sui-indexer-alt-framework = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4", default-features = false }
move-core-types           = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
move-binary-format        = { git = "https://github.com/MystenLabs/sui", rev = "c8755d9c05209a22d91c07df76458992defd99c4" }
```

`move-core-types` and `move-binary-format` live in the same repository
(`external-crates/move/crates/…`) and are workspace members, so a single git source covers everything;
`Cargo.lock` is committed for reproducibility.

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
| Layout resolution | `sui_rpc_resolver::package_store::RpcPackageStore::new(url).with_cache()` + `sui_package_resolver::Resolver::new(store)` → `Resolver::type_layout(TypeTag) -> MoveTypeLayout` | `crates/sui-package-resolver/src/lib.rs:404` |
| Bytecode / signatures | `Resolver::package_store().fetch(addr) -> Arc<Package>`; `Package::module(name) -> Module`; `Module::{functions, function_def} -> FunctionDef` | `lib.rs:305,749,1059,1076` |
| Type canonicalisation | `Resolver::canonical_type(TypeTag) -> TypeTag`, `Resolver::abilities(TypeTag)` | `lib.rs:382,432` |
| BCS → typed, single pass | `move_core_types::annotated_visitor::{Visitor, Traversal, ValueDriver, StructDriver, VecDriver, VariantDriver, NullTraversal, visitor_default!}` | `…/move-core-types/src/annotated_visitor.rs` |
| Path projection | `move_core_types::annotated_extractor::{Extractor, Element::{Field, Index, Type, Variant}}` | `…/annotated_extractor.rs` |
| BCS → annotated tree (dumps) | `sui_types::object::bounded_visitor::BoundedVisitor::{deserialize_value, deserialize_struct}` | `crates/sui-types/src/object/bounded_visitor.rs:81,96` |
| BCS → JSON (cross-check) | `sui_rpc_resolver::json_visitor::JsonVisitor::deserialize_value(bytes, &layout)` | `crates/sui-rpc-resolver/src/json_visitor.rs:60` |
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
| `birdai-move` | Protocol newtypes (`I32`, `I128`, `OptionU64`) and the `MoveStruct` derive that turns a layout-driven visitor into a typed struct. |
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
                  (I32/I128/OptionU64, MoveStruct derive,
                   reusable visitors, dump renderer)
                                |
        =========== Sui crates (pinned git rev) ===========
   sui-types · sui-package-resolver · sui-rpc-api · sui-rpc-resolver
   sui-indexer-alt-framework · move-core-types · move-binary-format
```

Dependency edges only point downward; `birdai-amm` has **no I/O and no Sui dependency** beyond
`move-core-types::u256` (it is pure integer math and can be property-tested in microseconds).

### 4.1 The four seams

```rust
/// 1. Where typed state comes from. Implemented by the gRPC client, the checkpoint stream, and fixtures.
#[async_trait]
pub trait ObjectSource: Send + Sync {
    async fn object(&self, id: ObjectID, version: Option<SequenceNumber>) -> Result<Object>;
    async fn object_at_checkpoint(&self, id: ObjectID, cp: u64) -> Result<Object>;
    async fn dynamic_fields(&self, parent: ObjectID, cursor: Option<Bytes>) -> Result<Page<DynamicField>>;
}

/// 2. Where layouts come from, with package-upgrade awareness layered on top.
#[async_trait]
pub trait LayoutSource: Send + Sync {
    async fn layout(&self, tag: &StructTag) -> Result<Arc<MoveTypeLayout>>;
    async fn canonical(&self, tag: &StructTag) -> Result<StructTag>;
    async fn module(&self, pkg: ObjectID, module: &str) -> Result<Arc<Module>>;
}

/// 3. Where raw objects come from (checkpoint stream, validator, or replay file).
pub trait RawObjectSource {
    fn id(&self) -> ObjectID;
    fn version(&self) -> SequenceNumber;
    fn struct_tag(&self) -> Option<StructTag>;
    fn contents(&self) -> &[u8];
    fn owner(&self) -> &Owner;
}

/// 4. How a typed venue is produced from bytes + layout.
pub trait Venue: Sized + Send + Sync + 'static {
    const KIND: VenueKind;
    fn decode(bytes: &[u8], layout: &MoveTypeLayout, ctx: &DecodeCtx<'_>) -> Result<Self>;
    fn price_state(&self) -> Option<PriceState>;      // powers classification probe 2 and pricing
}
```

`RawObjectSource` is implemented by `sui_types::object::Object` directly (a two-line impl), which is the
whole point: the checkpoint object **is** `sui_types::object::Object`, so our boundary is
`Object` → `Venue`, with bytes/tag/version as the only inputs. §8.4 shows what changes when the source is
a validator.

---

## 5. Task 1 — Decode

### 5.1 Fetch

Primary path is **gRPC v2** (`sui_rpc_api::Client`), not GraphQL:

```rust
let mut client = sui_rpc_api::Client::new("https://fullnode.mainnet.sui.io:443")?;
let obj: sui_types::object::Object = client.get_object_with_version(pool_id, version)?.into();
let mv = obj.data.try_as_move().expect("move object");
let tag: StructTag = obj.struct_tag().unwrap();
// contents: &[u8] == mv.contents()
```

Why gRPC over GraphQL: it returns a **native `sui_types::object::Object`** (the same type the checkpoint
stream carries), it supports read masks, batching and dynamic-field paging, and `sui_rpc_resolver`
already wraps it as a `PackageStore`. GraphQL is kept as a cross-check channel (its `contents { json }` is
the "pre-parsed JSON" the task allows for verification) and as a fallback for
`Address.dynamicFields` on inner UIDs — verified working during research.

### 5.2 Layout

```rust
let store  = sui_rpc_resolver::package_store::RpcPackageStore::new(RPC_URL).with_cache();
let resolver = sui_package_resolver::Resolver::new_with_limits(
    store,
    sui_package_resolver::Limits {
        max_type_argument_depth: 16,
        max_type_argument_width: 16,
        max_type_nodes: 256,
        max_move_value_depth: 128,
    },
);
let layout: MoveTypeLayout = resolver
    .type_layout(TypeTag::Struct(Box::new(pool_tag.clone())))
    .await?;
```

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
  M1 includes an integration test that cross-checks the gRPC and GraphQL pages element-for-element.

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
  tick_manager.ticks.id                        ⇒ 0x7f07284d…  (inner UID; 653 dynamic-field children)
```

Every offset is taken from `ValueDriver::{start, position}` and is asserted in tests against
`bcs::to_bytes` round-trips.

`--check` additionally fetches the node's `contents { json }` and deep-diffs it against our decode, so the
"do not use the node's pre-parsed JSON as your decoder, it is fine for checking your answers" requirement
is satisfied mechanically rather than by claim.

---

## 6. Task 2 — Classify

### 6.1 The test

Field names and "it holds balances" are explicitly disqualified. The test is **structural and
behavioural**, and every clause is mechanically decidable:

> **Price-discovery test.** An object `O` of type `T` is a trading venue with on-chain price discovery iff
> all three hold.

**(1) Inter-asset swap entry (static, from bytecode).**
Let `M` be the module that defines `T`, and `ps = T.type_params`. There must exist a function
`f ∈ M` with `f.visibility == Public` (or `f.is_entry == true`) such that, for two **distinct indices**
`i ≠ j` into `f.type_params`:

* some parameter is `OpenSignature { ref_: Some(Mutable), body: Datatype(T, args) }` — a `&mut T`;
* some parameter is `Datatype(0x2::coin::Coin | 0x2::balance::Balance, [TypeParameter(i)])`;
* some parameter is `Datatype(0x2::coin::Coin, [TypeParameter(i)])` **and** some return is
  `Datatype(0x2::coin::Coin | 0x2::balance::Balance, [TypeParameter(j)])` with `j ≠ i`.

Read directly off `FunctionDef { visibility, is_entry, type_params, parameters: Vec<OpenSignature>, return_ }`
(`sui-package-resolver/src/lib.rs:227-243`), with `OpenSignatureBody::Datatype(DatatypeKey, Vec<OpenSignatureBody>)`
and `OpenSignatureBody::TypeParameter(u16)`. No field-name heuristics, no registry, no ABI-string parsing.

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
function of the object's own fields. Probe (1) matches `pool::swap`: `&mut Pool<A,B>` plus `Coin<B>` in and
`Coin<A>` out. Probe (2) holds concretely — transaction T moves `current_sqrt_price` from
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

and the **crossing branch is asserted dead for T**: `S' < sqrt_price(71180)`, so no tick is crossed and
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
Arc<Checkpoint>
 └─ for each ExecutedTransaction (par_iter over the checkpoint)
      ├─ effects.published_packages()            → LayoutRegistry::on_package_published(ids)
      ├─ effects.object_changes()                → for each change {id, in_v, out_v}:
      │     ├─ out_v.is_none()  ⇒ tombstone(id)                 // deleted / wrapped
      │     └─ else ⇒ out = object_set[(id, out_v)]
      │            ├─ if out.owner is ObjectOwner(parent) ⇒ ChildIndex.upsert(parent, out)
      │            ├─ else if out.struct_tag() ∈ registered venue tags ⇒ decode → Slot::publish
      │            └─ else ⇒ ignore (we index only what we price)
      └─ effects.unchanged_consensus_objects()   → liveness bookkeeping only
 └─ commit(): one ArcSwap swap publishes the new CheckpointCursor  (single writer)
```

* **Readers see a consistent snapshot**: each slot is `ArcSwap<Versioned<T>>`; the checkpoint cursor is
  published last, so a reader either sees the whole checkpoint or none of it.
* **Parallel decode**: `rayon` over the changed objects; the Sui crates are `Sync` where it matters
  (`Resolver`, `Package`, `CompiledModule`, `MoveTypeLayout`), and `Resolver::type_layout` is `async`, so
  we pre-resolve the distinct tags of the checkpoint in one batch, then decode synchronously.
* **Lock-free reads**: `scc::HashMap<ObjectID, Slot>`, and the tick index is a `BTreeMap<i32, Tick>` behind
  the same `ArcSwap` publication, so a pricing thread never blocks a writer.

### 8.3 The three hard problems

**New pools.** Venue identity is by **`StructTag` shape + the §6 classifier**, not by an allow-list. On
first sight of a tag we run the static probes; if it is a venue we register a `VenueKind`, compile its
`Extractor` paths, and start tracking it. A newly deployed pool of a known protocol is picked up by its
defining package address; a brand-new protocol is picked up if it passes the probes, and is flagged
`unverified` until its price agrees with a reference venue to within a tolerance.

**Dynamic-field churn.** Children are separate objects whose owner is an **inner UID**, so they do not
look like children at all in the change set. `ChildIndex` therefore keys on
`derive_dynamic_field_id(parent_uid, key_type, key_bcs)` (available from `sui_types::dynamic_field`), and
three arrival paths are handled uniformly:

1. **The child object itself appears** in the changed set → decode with
   `sui_types::dynamic_field::visitor::FieldVisitor`, which yields `Field { name_bytes: &'b [u8],
   value_bytes: &'b [u8], .. }` — zero-copy, and exactly enough to key the child without materialising it;
2. **The child is deleted or wrapped** (`effects.deleted()` / `wrapped()`) → patch the parent from the
   `ObjectRef` alone; no contents needed;
3. **The parent's `Table`/`Bag`/`SkipList` `size` field changes** → used **only as a consistency
   assertion** against `ChildIndex::len()`. A mismatch is a metric plus a forced resync of that parent,
   because a drifted child index is the failure mode that silently misprices everything downstream.

Backpressure matters here: a single Navi `Storage` has ~999k `user_info` children and pool A has 653 tick
nodes, so `ChildIndex` is **bounded per parent** by an LRU tail and only tracks parents we actually price
(`TrackedParent` registration). Tick nodes are the exception — they are dense enough (653) to index fully,
which is what makes tick-range pricing O(log n).

**Package upgrades that change layouts.** This is the gap `PackageStoreWithLruCache` does not close: it
caches `Package` by storage id and re-fetches on demand, but it has **no invalidation hook**, so a cached
`MoveTypeLayout` for a dependent type survives a package upgrade. `birdai-resolve` adds:

* cache key `(canonical StructTag, defining_package_version)`, not `StructTag` alone;
* a `LayoutRegistry::on_package_published(ids)` callback driven by `effects.published_packages()` and by
  `MovePackage` objects observed in the change set, which invalidates every cached layout whose canonical
  tag's defining address is in the published/upgraded set — **transitively**, since pool A depends on three
  packages (`pool`, `i32`, `skip_list`) and an upgrade to any of them changes the pool's instantiated
  layout;
* a **fingerprint** (`blake3` over the compiled layout, including field names and tags) stored next to each
  published state, so a checkpoint replay reproduces the exact layout that was live at that checkpoint.

Typed states are built by **field name** (§5.3), so an appended or reordered field needs no code change;
a renamed or removed field surfaces as `DecodeError::MissingField`, which is routed to an alert instead of
silently producing a stale price. That is the deliberate boundary: *layout drift is loud, not silent.*

### 8.4 The boundary, and what changes inside a validator

The boundary is exactly three things — **bytes, tag, version** — entering a typed state:

```
raw object (BCS bytes + StructTag + SequenceNumber + Owner)
   → LayoutSource::layout(tag)                       [layout, cached, upgrade-aware]
   → Venue::decode(bytes, layout, ctx)               [one pass, name-matched, no tree]
   → Versioned<VenueState>                           [published atomically per checkpoint]
```

| | Checkpoint stream | Inside a validator |
|---|---|---|
| Raw objects | `sui_types::object::Object` from `Checkpoint::object_set` | the same type from the object store / `InputObjects` |
| Version truth | Final and ordered; a checkpoint commits atomically | **Provisional**: state changes before consensus, so the manager needs `begin_tx / apply / commit_or_abort` with rollback on re-execution, and must not publish a checkpoint cursor until the effects are durable |
| Layouts | `RpcPackageStore` over the network; async; evictable | straight out of the validator's `ModuleCache`; **synchronous**, and a package upgrade is visible the instant the publish executes |
| BCS provenance | From the wire; validated by the checkpoint digest | The very buffer the VM executed against — already trusted, so decode can skip validation and borrow directly, no copy |
| Children | Must be re-associated by owner/effects | The VM hands over the child objects it loaded, so `ChildIndex` can be built from `InputObjects` directly |
| Failure mode | Lag or a gap → resync from a checkpoint | Re-org or aborted execution → MVCC rollback |

Concretely, switching source means: implement `RawObjectSource` for the validator's object store (the
`sui-types` impl is already the same code), make `LayoutSource` synchronous behind the same trait (or keep
`async` with a ready future), and add the two-phase commit to `StateManager`. **No venue, math, tick or
decode code changes.** That is the payoff of putting the boundary at bytes+tag+version rather than at "an
HTTP client".

### 8.5 Ordering hazard worth calling out

A checkpoint's `object_set` can contain several versions of the same object, and `effects.object_changes()`
is per transaction. Applying changes in `Checkpoint::transactions` order and asserting
`slot.version < new.version` catches both out-of-order application and the "same object mutated twice in
one checkpoint" case; violating it is an error, not a silent last-writer-wins.

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

* **Golden fixtures** — A, B, C, the tick child and checkpoint `320577815`'s relevant transactions:
  bytes + layout + expected field dump with offsets. `cargo test` never touches the network.
* **Reproduction as a test** — `assert_eq!(reproduce(pre_state, &tx)?, 81_168_759)`.
* **Cross-check** — deep-diff our decode against the node's `contents { json }` (explicitly permitted) and
  against `JsonVisitor::deserialize_value`.
* **Offset invariants** — for every leaf, `&bytes[start..position]` re-decoded standalone equals the leaf
  value; `Cursor`-style skip and `Extractor` selection must agree on where every field ends.
* **`proptest`** in `birdai-amm`:
  * `mul_div_floor`/`mul_div_ceil` vs a `ruint::U512` reference (dev-dependency) and vs an `f64` oracle;
  * `amount_out` strictly increasing in `amount_in`, strictly decreasing in `fee_rate`;
  * round-trip: `swap(a→b)` then `swap(b→a)` never profits the trader, for random `L`, `S`, fee;
  * tick loop terminates and conserves `L` at every boundary crossing;
  * `CheckedU256` never wraps — for `L, S, amount` near the proven bounds the result is either exact or
    `AmmError::Overflow`, never a wrong number.
* **`proptest`** in `birdai-tick`: random tick sets, `Locate::Score` agrees with an exhaustive scan; the
  skip-list walk never visits a node twice; `head`/`tail`/`size` are consistent.
* **Boundary cases** — empty vector, `Option` present/absent, `OptionU64 { is_none: true }` with a stale
  `v`, `L = 0`, `S = 0`, `u64::MAX` balances, single-element skip list, enum variant 0 and last.
* **Classifier tests** — A passes all three probes; B fails (1)(2); C fails (1)(2)(3); plus a synthetic
  package that has two `Balance` fields and no swap, which must **not** classify as a venue.
* **Integration** — gRPC vs GraphQL dynamic-field pages for the tick inner UID, element for element.
* `just format && just lint && just test && just mutation` (cargo-mutants), zero surviving mutants in
  `birdai-amm` and `birdai-tick`.

---

## 11. Milestones (quality-first)

No deadline. Each milestone is independently useful and leaves the repo green.

**M0 — Skeleton and pins.** Workspace with the pinned git deps; `rust-toolchain.toml` 1.96.1;
`just deps-check`; `ObjectSource`/`LayoutSource` traits with a gRPC implementation; fixture capture tool.
*Exit:* `cargo run -- fetch --all` writes the three objects, their layouts and checkpoint 320577815 to
`fixtures/`.

**M1 — Decode.** `birdai-move`: `I32`/`I128`/`OptionU64`, `Dump` visitor, `MoveStruct` derive.
`birdai-tick`: skip-list indexing and `Locate`. Venue visitors for A/B/C.
*Exit:* `decode --all` prints offset-annotated dumps; `check` deep-diffs against node JSON; gRPC/GraphQL
dynamic-field cross-check passes.

**M2 — Classify.** `birdai-venue::Classifier` with the three probes, the oracle deny-set, and evidence
rendering.
*Exit:* `classify --all` prints per-probe evidence and the verdicts in §6.2.

**M3 — Recreate.** `birdai-amm`: `CheckedU256`, `tick_math` (`sqrt_price_at_tick`, exactly as
`1.0001^(t/2)` with the bit-decomposition trick), `sqrt_price_math`, `swap_math`, and the multi-tick loop.
*Exit:* `reproduce` prints L, S, tick range, the step, the output, and `Δ`; the golden test is 81 168 759;
proptest suite green.

**M4 — State.** `birdai-state`: `StateManager`, `ChildIndex`, `LayoutRegistry` with transitive
invalidation, `Slot`/`ArcSwap` publication, two-phase commit hooks for the validator path.
*Exit:* `follow --from 320577815 --to 320577900` tracks the pool live and prints tick-index delta; package
upgrade invalidation covered by a synthetic test.

**M5 — Polish.** README (the graded §2/§3/§4 answers), `--offline` fixture mode, benches, `just ci` green,
mutation score reported.

---

## 12. Risks

| Risk | Mitigation |
|---|---|
| Sui git dep build time / disk on a fresh clone | Pinned rev + committed `Cargo.lock`; `kache` wrapper; `[profile.dev.package."*"] opt-level = 2`; README states the one-time network requirement |
| Public fullnode gRPC / GraphQL rate limits during a demo | Fixtures + `--offline`; the state manager takes a `RawObjectSource`, so the demo can replay a checkpoint file |
| Public checkpoint object stores retain only ~30 days | Checkpoint `320577815` is fetched once and committed as a fixture |
| Cetus package upgraded between research and grading | `calibrate` re-derives the fixed-point format from `S` and `tick`; typed decode is name-based; the layout cache is version-keyed |
| `Resolver` limit defaults differ from the pool's nesting depth | Limits set explicitly and asserted; `Pool` is shallow (< 20 nodes) |
| `PackageStoreWithLruCache` re-fetch semantics under `--offline` | Fixture store implements `PackageStore` directly, so the resolver path is exercised end-to-end offline |
| Tick child not enumerated under `--offline` | All 654 tick nodes captured to fixtures at the relevant versions |

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
