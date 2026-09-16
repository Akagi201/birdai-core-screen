# Birdai Core screen: decode, classify, recreate

Three Sui mainnet objects decoded from raw BCS against a layout resolved from on-chain bytecode,
including one tick child of the CLMM; the three classified by whether they discover prices on chain;
and transaction T recomputed from the pool state it consumed, to the exact base unit.

```bash
cargo run -- reproduce   # 81_168_759 USDC out, difference 0
cargo run -- decode      # field-by-field dump of A, B, C and one tick child of A
cargo run -- classify    # the price-discovery test and its evidence
cargo run -- calibrate   # derives the pool's fixed-point format from its own state
cargo run -- follow      # keeps venue state current from the checkpoint stream
cargo run -- fetch --out fixtures   # captures a fixture set

cargo run -- --fixtures fixtures reproduce   # the same numbers, with no network
```

Rust only. The Sui crates come from `github.com/MystenLabs/sui` pinned to one revision
(`f0831497799964f2e364a20a380963fc6b4872c5`), toolchain 1.96.1, so the first build clones that
monorepo and takes a while. `just deps-check` verifies every git dependency names the same `rev`.

A run talks to two mainnet endpoints because no single public one does both jobs:
`fullnode.mainnet.sui.io` serves objects and the dynamic-field index but keeps only a bounded window
of history, while `archive.mainnet.sui.io` keeps the whole history but has no `StateService` at all.
Checkpoints and versioned object reads go to the archive, everything else to the fullnode.
`--rpc-url`, `--archive-url` (an empty string opts out of the split) and `--api-key` override that.

## Offline replay

`fixtures/` is 1.2 MB captured from mainnet: 658 object versions across 656 ids (pool A at three
versions, B, C, and 653 tick nodes), 12 packages of bytecode, 9 resolved layouts, and the checkpoint
that carried T. `cargo run -- fetch --out fixtures` recaptures it.

It stores raw Sui types only: `Object` BCS, `MoveTypeLayout` JSON, and the parts of a `Checkpoint`
(Sui does not make that type `Serialize`, so it is reassembled from summary, contents, transactions
and object set). No derived answers anywhere in it. That is the point: an offline `reproduce` walks
the same decoders, the same layout resolution and the same swap math as an online one, so its
printing `81_168_759` is a check rather than a replay of a stored number. `cargo test` runs against
the committed set — pool A at version 995 150 484 decodes, the checkpoint reassembles with T and its
`pool_script_v2::swap_b2a` call intact, tick children are reachable through their inner UID, layouts
round-trip out of JSON, and the state layer replays checkpoint 320 577 815 with a re-apply as a
no-op.

Two things in there are a snapshot rather than the chain. The captured checkpoint keeps only the 7 of
33 transactions whose effects touch pool A, so its object set is a subset. And the tick counts (653
nodes, tick 72172) are as of the capture; the pool trades continuously, so they were history the
moment the files were written. What does not drift is version-pinned: pool A at 995 150 484,
transaction T, and the 81 168 759 they imply.

---

## 1. Decode

`cargo run -- decode` prints every field of every object with the **BCS byte range each value came
from**, taken from `move_core_types::annotated_visitor`'s `ValueDriver::{start, position}`. The
decoder implements that visitor directly, so there is no intermediate `MoveValue` tree, and a field
appended or reordered by a package upgrade is skipped rather than breaking the decode. Byte ranges
are layout-stable; the reserve and tick values move, so read them as the shape of the output.

```
0x1eabed72…::pool::Pool<…::usdc::USDC, 0x2::sui::SUI>
  coin_a                       [   32..   40] =
    0x2::balance::Balance<0xdba34672…::usdc::USDC>
      value                        [   32..   40] =
        251800662977  <u64>
  current_tick_index           [   92..   96] =
    0x714a63a0…::i32::I32
      bits                         [   92..   96] =
        72172  <u32>
  tick_manager                 [  144..  374] =
    0x1eabed72…::tick::TickManager
      ticks                        [  148..  374] =
        0xbe21a061…::skip_list::SkipList<…::tick::Tick>  {dynamic-field container: entries are separate objects}
          id                           [  148..  180] =
            …7f07284d6d6373a1b32d8f721991c3c17aa2f895abcc34e0d5990a8a99aaf2ae  <address>
```

The tick child is fetched through that inner UID — not the pool's own id, which returns no children
at all — and its dump shows the skip list's key scheme:

```
nearest initialised tick above 72172:
  score            515836
  tick index       72200   (score - 443636 = 72200)
  sqrt_price       681780251452874908957
  liquidity_net    -2737953659066
  nexts            [515976, 515986, 516006]
```

Where the four standard cases needed handling:

- **Generics.** Both `Pool<USDC, SUI>` parameters are `phantom`, so they occupy zero BCS bytes and
  the layout is byte-identical for every instantiation. The fully instantiated `StructTag` is the
  cache key, and the layout comes back substituted, with `Balance<USDC>` and `Balance<SUI>` distinct.
- **`Balance<T>`** is `{ value: u64 }` inlined, never a child object, so it decodes one level down as
  a `u64`.
- **`Option`, in two flavours inside the same object.** The pool's `position_manager` holds a
  `LinkedTable` whose `head`/`tail` are std `0x1::option::Option<ID>` — a vector of length 0 or 1.
  The tick skip list's `head` is Cetus's own `option_u64::OptionU64`, `{ is_none: bool, v: u64 }`,
  nine bytes with the payload always present. A layout-driven decoder cannot mix them up: one layout
  says `vector<ID>`, the other a two-field struct. A decoder that matched names could.
- **`Table`, `Bag`, `SkipList`.** The parent carries `{ id, size }` and nothing else — Navi's
  `Storage` is 155 bytes while its reserves and ~999k user positions live in dynamic fields, and
  Volo's `vaults` is a `Table` whose size is all I read. The skip list is the one container whose
  children I do read, because the tick set is what a quote prices against.
- Two smaller notes: Cetus's `I32`/`I128` are `{ bits: u32 }`/`{ bits: u128 }` living in a **different
  package** from the pool, reinterpreted as two's complement (`liquidity_net` is routinely negative);
  and enums, which none of A/B/C contains, are supported by the visitor and rendered `@variant`.

---

## 2. Classify

### The test

Field names and "it holds balances" are not evidence. What I applied is a structural and behavioural
test, decided from package bytecode and from two versions of the object:

> An object `O` of type `T` is a trading venue with on-chain price discovery iff all three hold.
>
> 1. **Inter-asset swap entry.** Some public or `entry` function mutably borrows the venue itself —
>    `&mut T<…>` — and carries asset legs on **two different type parameters of that borrow**: a
>    `Coin<X>`/`Balance<X>` leg and a `Coin<Y>`/`Balance<Y>` leg, `X ≠ Y`. Read from
>    `FunctionDef`/`OpenSignatureBody`, so nothing about names or balances enters it.
> 2. **Endogenous price state.** `O` carries a price variable that its own fields determine. Where a
>    transaction that used the entry from (1) exists, that variable also moved in the direction the
>    net flow implies, with no oracle object among the transaction's inputs.
> 3. **No imported price.** Neither the object's field types, nor the modules its package links, nor
>    the entry's signature name a price feed.

Clause 1 is what separates a venue from a vault: a vault can hold two assets and still have no
function that exchanges them, because it has no price at which to do so. Clause 2 separates *has a
price field* from *discovers a price*. Clause 3 separates discovery from import.

Clause 1 forces one detail into the open: **Cetus's `pool` module has no swap.** Nothing in it
exchanges `Coin<T0>` for `Coin<T1>`; the entry that moved pool A is
`0xae9c208c…::pool_script_v2::swap_b2a`, in a sibling package. So the probe scans the defining
package *and* resolves the `package::module::function` the chain actually executed, anchored on pool
A's own type: an entry that does not borrow `&mut Pool<T0, T1>` is some other pool's business.

### A — `0x1eabed72…::pool::Pool<USDC, SUI>`, Cetus CLMM: yes

The pool's own state carries `liquidity`, `current_sqrt_price` (Q64.64) and `current_tick_index`,
plus a skip list of initialised ticks with their `liquidity_net` — 653 of them in the captured set,
and the count drifts as the pool trades. The marginal price is a function of those fields and of
nothing else. Clause 1 is satisfied by the entry the chain executed:
`(&GlobalConfig, &mut Pool<T0, T1>, Coin<T0>, Coin<T1>, bool, u64, u64, u128, &Clock, &mut
TxContext)`, which the classifier checks *is* a mutable borrow of pool A with legs on its own two
parameters. Clause 2 holds concretely: across T, `current_sqrt_price` rose from
`647_308_812_393_509_050_120` to `647_324_162_169_833_037_484` while `coin_b` rose and `coin_a`
fell, and none of the transaction's 9 input objects is a price feed. Clause 3: no linked module,
field type or signature in the pool's package names a feed. Every unit of price in this object is
discovered by trading against it.

### B — `0x549e8b69…::native_pool::NativePool`, Volo liquid staking: no

It is pool-shaped — a `pending` coin, a `collectable_fee` coin, a `vaults` table, a validator map —
and it does hold SUI. But clause 1 fails structurally and unconditionally: `NativePool` has no type
parameters, so no function on it can borrow generic state and exchange two of its own assets. Of the
77 functions in its package, the ones that touch coins are `stake` (SUI in, no coin out),
`unstake`/`mint_ticket` (CERT in), `burn_ticket` (a ticket in, SUI out) — each moves one asset
against a share claim. Volo's SUI↔VSUI rate is an accounting ratio that moves when rewards accrue or
validators are rebalanced, never when someone trades. Clause 2 fails as well, and here the code can
say so precisely: decoding the whole object yields no price variable at all. Two coin fields and a
ratio is a vault, not a venue.

### C — `0xd899cf7d…::storage::Storage`, Navi lending: no

Its entire BCS is 155 bytes and contains no balances: `reserves` and `user_info` are
`0x2::table::Table`s, so the object holds two `UID`s, two lengths and a version, while the 35
reserves and ~999k user positions live in dynamic fields. Clause 1 fails for the same structural
reason as B — `Storage` has no type parameters, so none of the 144 functions in its package can
exchange two of its own assets; `deposit`, `repay` and `liquidation_call` each take one `Coin<T>`
against a share.
Clause 2 fails too: nothing in those 155 bytes is a price. Clause 3 fails as well, and this is where
C differs from B: the storage package's modules link `oracle` from `calculator`, `dynamic_calculator`,
`lending` and `logic`. Asset values are imported and interest is a utilisation curve. A ledger with a
price feed attached is still not a place where price is discovered.

---

## 3. Recreate state and reproduce T

`cargo run -- reproduce` prints the whole derivation. The short version:

```
pre-state  pool A @ version 995_150_484   (reached two ways: by version, and as the object in
                                           checkpoint 320_577_815's deduplicated object set)
  liquidity        L   120_115_891_674_982
  current_sqrt_price S 647_308_812_393_509_050_120     (= 35.090681033302715 × 2^64)
  current_tick_index   71_162      fee_rate 500 (5 bps)      tick_spacing 10

step
  fee                 50_000_000      (100e9 × 500 / 1e6 — the fee the transaction states)
  amount_in_after_fee 99_950_000_000
  ΔS = ⌊in · 2^64 / L⌋ 15_349_776_323_987_364
  S' = S + ΔS         647_324_162_169_833_037_484

result
  amount_out          81_168_759      ← the chain's value
  difference          0
  steps               1               liquidity unchanged: 120_115_891_674_982
```

**Liquidity, price and range.** At the version T consumed, `L = 120_115_891_674_982` and
`S = 647_308_812_393_509_050_120`, in the tick the pool reports, 71 162. The pool's tick set can only
be read at its *current* version — T's version declares 650 ticks and 653 children exist today — so
the range I can read now (nearest boundary above: tick 71 190 at
`sqrt_price = 648_206_889_171_250_166_865`) may not be the range that was in force, and I do not lean
on it. What proves no tick was crossed is the pre-state alone: the reached price floors to the same tick
index as the starting price (71 162), and initialised ticks sit on multiples of `tick_spacing`, so no
boundary lies between the two prices. The command prints both that test and the live bracket, and
asserts that quoting with no tick boundaries at all gives the same `81_168_759` as quoting with all
653 children — which is why losing the historical tick set costs this reproduction nothing.

**Rounding, and the difference from the chain.** There is no difference: the recomputation equals
`81_168_759` exactly, and it is asserted as a test
(`birdai-amm::swap::tests::output_matches_the_chain_exactly`), not printed as a hope. The directions
that produce it:

| Step | Direction | Why |
|---|---|---|
| `fee = ⌊in · fee_rate / 1e6⌋` | floor | protocol constant |
| `ΔS = ⌊in · 2^64 / L⌋` | floor | smallest price improvement the input pays for — pool-favourable |
| `out = ⌊(L ≪ 64) · ΔS / (S · S')⌋` | floor | pool-favourable; the V3-lineage `getAmount0Delta(round_up = false)` |

Rounding `ΔS` **up** gives the same answer, because at this step width one unit of ΔS buys less than
one base unit of output (`one_unit_of_price_move_is_worth_less_than_one_base_unit_of_output`). So the
reproduction is exact *and* insensitive to that choice, which is a stronger statement than a lucky
match. A floating-point path has no way to place that floor exactly, so its last digit would not be
trustworthy — which is why the implementation is integer-only.

Two implementation notes that decide correctness:

- **`U256` is enough; no 512-bit arithmetic is needed.** Each reserve is a `Balance<T>`, i.e. a
  `u64`, so virtual reserves are `< 2^64` and `L = √(a_v·b_v) < 2^64`; with an input `< 2^64` that
  makes `ΔS < 2^128`, so the numerator `(L ≪ 64)·ΔS < 2^256` and the denominator `S·S' < 2^256` too.
  Arithmetic goes through a `CheckedU256` wrapper because `move_core_types::u256::U256`'s
  `Add`/`Sub`/`Mul` **wrap** and its `Div` panics on a zero divisor — in a pricing path that is a
  silent wrong answer, and an input outside the hypothesis fails loudly instead.
- **Tick prices are read, never recomputed.** Checking the captured set's 653 nodes against
  `⌊1.0001^(t/2)·2^64⌋` gives 651 exact and a worst case 7 units low (histogram
  `{-7: 1, -1: 1, 0: 651}`). No variant of the plausible decomposition reproduces every observed
  value, so the stored per-tick price is authoritative, and the closed form is used only to map a
  price back to a tick and to check that a node is what it claims — with a relative tolerance of
  `2^-48`, far looser than the deviation and still far tighter than the gap between adjacent ticks.

---

## 4. Design note — keeping state current (≤300 words)

State is kept current by one path: `StateManager::apply_checkpoint`, fed by
`sui_indexer_alt_framework::ingestion` — hybrid gRPC streaming, object-store backfill and
backpressure, no database. It records every `MovePackage` under both its new and its original id, so
the layout cache invalidates anything mentioning a moved package before this checkpoint's layouts are
resolved; resolves each tag once; decodes on the blocking pool; and
publishes one `Arc<VenueSlot>` per object, with version monotonicity: a replayed checkpoint is a
no-op, an out-of-order version is counted, not served.

Three problems dominate. *New pools* are found by `module::name` and confirmed by the resolved
layout's field set, because several mainnet packages define `pool::Pool` with different fields; the
name is a hint, the shape is the test. *Dynamic-field churn* is harder: tick nodes hang off the skip
list's inner UID, not the pool, so they are indexed by that owner, capped per parent. The declared
`size` and today's children are read at different versions and disagree in both directions, so the
skew is reported, and a vanished child leaves its parent's set. *Package upgrades* need the
cache keyed by layout dependencies: each cached layout records the packages it mentions and is
dropped when one moves.

The boundary in code is one tuple: object id, version, `StructTag`, layout fingerprint, BCS bytes.
Below it everything is `sui_types::object::Object`; above it everything is typed state that knows
nothing about RPC or checkpoints. Tick state is the exception: dynamic fields are fetched separately
and attached afterwards.

Inside a validator the boundary is the same but its properties invert: objects come from the executor
rather than a stream, BCS is trusted and can be borrowed rather than copied, layouts come
synchronously from `ModuleCache`, and state is provisional — publishing in place becomes a two-phase
commit with rollback. No venue, tick or pricing code changes.

---

## What running against mainnet changed

Each of these contradicted something that looked obvious first, which is why they are here:

1. **Cetus's `pool` module contains no swap.** Nothing in it has the inter-asset shape. A probe that
   only looked at the defining package would report a false negative on the clearest venue in the
   set, so the probe also resolves the entry the chain executed.
2. **`module::name` is not identity.** `follow` immediately met 27 objects named `pool::Pool` from
   other packages whose layouts differ; they fail the shape check and are counted as `unrecognised`,
   not as failures.
3. **A pool's children cannot be read at a historical version.** T consumed pool A at version
   995 150 484, which declares 650 ticks; enumerating today returns 653. The skew is reported, not
   swallowed, and the quote is proven without the tick set.
4. **On-chain tick prices are not exactly `⌊1.0001^(t/2)·2^64⌋`.** Two of 653 nodes differ, one by 7
   units. The stored values are treated as authoritative, which the swap math already did.
5. **`move_core_types::u256::U256`'s operators wrap** and its `Div` panics on zero. Nothing in its
   documentation says so; a pricing path cannot use them directly.
6. **Sui's two public mainnet endpoints are not interchangeable.** One serves the whole API with
   bounded history, the other the whole history with no `StateService`, so checkpoints and versioned
   reads go to the archive and everything else to the fullnode.
7. **`sui-indexer-alt-framework`'s `default-features = false` does not remove Diesel/Postgres** —
   `sui-indexer-alt-metrics` depends on `sui-pg-db` unconditionally. The framework is still the right
   choice (nothing else ships the ingestion stack), but the claim had to be corrected.

---

## Repository layout

```
bin/birdai                 CLI: decode | classify | reproduce | calibrate | follow | fetch
crates/birdai-move         protocol newtypes (I32, I128, OptionU64), the decoder toolkit, the dump
crates/birdai-resolve      gRPC object source, and a layout cache that invalidates on upgrade
crates/birdai-amm          tick math, delta math, exact-input swap, CheckedU256   (no I/O)
crates/birdai-tick         the Cetus tick skip list: decode, index, validate, bracket
crates/birdai-venue        typed venues and the price-discovery classifier
crates/birdai-state        checkpoint-driven in-memory venue state
docs/design.md             the full design, including the defects found while implementing it
```

`birdai-amm` and `birdai-tick` have no network dependency and are covered by unit and property tests.
`just test` runs everything (138 tests, offline fixture data included), `just lint` the pedantic
Clippy set with `-D warnings` over every target including tests, and `just deps-check` the Sui pin.
Mutation testing with `cargo mutants` reports **0 missed** in `birdai-tick/src/index.rs` (103 caught,
33 unviable) and 3 missed in `birdai-amm/src/swap.rs`, all three provably equivalent `||`→`&&`
mutations with the equivalence written out next to the code; every guard added along the way — a
wrong-side price limit, a stale boundary, a boundary exactly at the price, score/key consistency,
version monotonicity, fail-closed decoding — carries a test that fails when the guard is reverted.

Known limits, both deliberate: the layout cache's upgrade invalidation is exercised by unit tests on
the dependency graph rather than by a live upgrade observed in the stream (none happened while I was
running it), and the captured checkpoint is filtered to the transactions touching pool A, so
replaying an unrelated checkpoint would need a wider capture (`fetch` takes the filter as a closure
for exactly that reason).
