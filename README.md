# Birdai Core screen — decode, classify, recreate

Three Sui mainnet objects decoded from raw BCS with a resolved layout, classified by whether they
discover prices on chain, and used to recompute a real swap **exactly**.

```bash
cargo run -- reproduce    # recomputes transaction T: 81_168_759 out, Δ = 0
cargo run -- decode       # field-by-field dump of A, B, C and a tick child of A
cargo run -- classify     # the on-chain price-discovery test, with its evidence
cargo run -- calibrate    # derives the pool's fixed-point format from its own state
cargo run -- follow       # keeps venue state current from the live checkpoint stream

# ...or with no network at all, from the committed fixture set:
cargo run -- --fixtures fixtures reproduce
```

Rust only. The Sui crates are taken from `github.com/MystenLabs/sui` pinned to one revision
(`f0831497799964f2e364a20a380963fc6b4872c5`), toolchain 1.96.1. The first build
clones that monorepo and takes a while; `docs/design.md` explains what is reused and why.
`just deps-check` verifies the pin: every git dependency names a `rev`, and all revs are identical.

By default the run talks to **two** mainnet endpoints, because no single public one does both jobs:
`fullnode.mainnet.sui.io` for latest objects and the dynamic-field index, `archive.mainnet.sui.io`
for checkpoints and historical object versions (a fullnode's retention reaches neither checkpoint
320 577 815 nor pool A's version 995 150 484, and the archival node has no `StateService` at all).
Override with `--rpc-url` / `--archive-url`; `--archive-url ""`
opts out of the split. Hosted providers that require authentication take `--api-key`.

## Offline replay

`fixtures/` holds 1.2 MB captured from mainnet: 658 object versions across 656 ids (pool A at
three versions, B, C, and 653 tick nodes), 12 packages of bytecode, 9 resolved layouts, and a
filtered checkpoint. Recapture with `cargo run -- fetch --out fixtures`.

The fixture set stores **raw Sui types**, not derived answers — `Object` BCS, `MoveTypeLayout` JSON,
and the parts of `Checkpoint` (which Sui deliberately does not make `Serialize`, so it is rebuilt from
summary, contents, transactions and object set). Nothing in it is a pre-computed result. That is the
point: `reproduce --fixtures fixtures` walks the same decoders, the same layout resolution and the same
classifier as an online run, so its asserting the same `81_168_759` is a real check rather than a
replay of a stored number. `cargo test` exercises the committed set directly: the fixture tests
assert that pool A at version 995 150 484 decodes, the checkpoint reassembles with transaction T and
its `pool_script_v2::swap_b2a` call intact, tick children are reachable through their inner UID,
layouts round-trip out of JSON, and package bytecode still carries its modules — and the state
tests replay checkpoint 320 577 815 through `StateManager` offline, asserting pool A is tracked at
its output version and that re-applying the checkpoint is a no-op.

Two honesty notes are written into the manifest: the captured checkpoint keeps only the 7 of 33
transactions whose effects touch pool A, so its `object_set` is a subset; and tick-node counts can
drift between a pool's declared `size` and what enumeration returns, because dynamic fields can only
be listed as of the present. A third is worth stating plainly: the pool trades continuously, so any
count in this file — 653 tick nodes, tick 72172, the histogram below — is already history by the
time you read it. What does not drift is version-pinned: pool A at 995 150 484, transaction T, and
the `81_168_759` they imply.

---

## 1. Decode

`cargo run -- decode` prints every field of every object with the **exact BCS byte range** each value
came from, taken from `move_core_types::annotated_visitor`'s `ValueDriver::{start, position}`. The
decoder implements that visitor directly, so there is no intermediate `MoveValue` tree and a field
that is appended or reordered in an upgrade is skipped rather than breaking the decode. Byte
ranges are layout-stable; the reserve and tick values below move as the pool trades, so treat the
numbers as the shape of the output rather than as constants.

```
0x1eabed72…::pool::Pool<…::usdc::USDC, 0x2::sui::SUI>
  coin_a                       [   32..   40] =
    0x2::balance::Balance<0xdba34672…::usdc::USDC>
      value                        [   32..   40] =
        256765445431  <u64>
  current_tick_index           [   92..   96] =
    0x714a63a0…::i32::I32
      bits                         [   92..   96] =
        71986  <u32>
  tick_manager                 [  144..  374] =
    0x1eabed72…::tick::TickManager
      ticks                        [  148..  374] =
        0xbe21a061…::skip_list::SkipList<…::tick::Tick>  {dynamic-field container: entries are separate objects}
          id                           [  148..  180] =
            …7f07284d6d6373a1b32d8f721991c3c17aa2f895abcc34e0d5990a8a99aaf2ae  <address>
```

The tick child is fetched through that inner UID, and its own output shows the skip list's key scheme
(values below are from the captured run; the pool keeps trading, so live ticks sit higher today):

```
nearest initialised tick above 72172:
  score            515836
  tick index       72200   (score - 443636 = 72200)
  sqrt_price       681780251452874908957
  liquidity_net    -2737953659066
  nexts            [515976, 515986, 516006]
```

### Where the four standard cases bite

| Case | What the objects actually do | How it is handled |
|---|---|---|
| **Generics** | `Pool<USDC, SUI>` — both parameters are `phantom`, so they occupy **zero** BCS bytes and the layout is identical for every instantiation. | The fully instantiated `StructTag` is the cache key; the layout comes back substituted, with `Balance<USDC>` and `Balance<SUI>` distinct. |
| **`Balance<T>`** | `{ value: u64 }`, inlined. Never a child object. | Decoded one level down as a `u64`. |
| **`Option` vs Cetus's `OptionU64`** | The pool contains **both**: `LinkedTable::head` is std `Option<ID>` (a `vector<T>`, one length byte), while the skip list's `head` is `option_u64::OptionU64 { is_none: bool, v: u64 }` — 9 bytes, payload always present. | The layout says `struct`, so a layout-driven decoder cannot confuse them. A decoder that pattern-matched names would. |
| **`Table` / `Bag` / `SkipList`** | The parent carries only `{ id: UID, size: u64 }`. Navi's `Storage` is 155 bytes for 35 reserves and ~999k user positions. | Treated as a container: the dump labels it, `size` is used as an assertion against the children actually found, and children are read separately. |
| **`I32` / `I128`** | Cetus's signed types are `{ bits: u32 }` / `{ bits: u128 }`; the `I32` lives in a **different package** from the pool. | Reinterpreted as two's complement. A mainnet `liquidity_net` reads `340282366920938463463374605480910926342`, i.e. negative. |
| **Enums (bytecode v6)** | Unused by A/B/C. | Supported by the visitor framework; the dump renders `@variant`. |

---

## 2. Classify

### The test

Field names and "it holds balances" are not evidence. The test applied is structural and
behavioural, and every clause is mechanically decidable:

> An object `O` of type `T` is a trading venue with on-chain price discovery **iff** all three hold.
>
> 1. **Inter-asset swap entry.** There is a callable function that **mutably borrows `O`'s generic
>    state** — a `&mut D<…T…>` whose type arguments include `T`'s own type parameters — and carries
>    asset legs on **two different** of those type parameters. Nothing about names or balances enters
>    it: it is read from `FunctionDef`/`OpenSignatureBody` in package bytecode.
> 2. **Endogenous price state.** `O` carries a price variable that is a pure function of its own
>    fields, and across a transaction that used the entry from (1) it moved **in the direction the
>    flow implies**, with no oracle object among that transaction's inputs.
> 3. **No price imported from outside.** Neither `O`'s layout nor its package's module graph
>    references a price feed.

Clause (1) is what separates a venue from a vault: a vault can hold two assets and still have no
function that exchanges them, because it has no price at which to do so. Clause (2) is what separates
*has a price field* from *discovers a price*. Clause (3) separates discovery from import.

One detail clause (1) forces into the open: **Cetus's `pool` module has no swap.** Its 77 functions
were scanned and none exchanges `Coin<T0>` for `Coin<T1>`; the entry that moved pool A is
`0xae9c208c…::pool_script_v2::swap_b2a`, in a sibling package. So the probe scans the defining
package *and* resolves the `package::module::function` the chain actually executed, using the
signature of the function that really ran as the strongest available evidence.

### Object A — `0x1eabed72…::pool::Pool<USDC, SUI>` (Cetus CLMM): **yes**

The pool's own state carries `liquidity`, `current_sqrt_price` (Q64.64) and `current_tick_index`,
plus a skip list of initialised ticks with their `liquidity_net` (653 in the captured set; the
count drifts as the pool trades). The marginal price is a pure
function of those fields. Clause (1) is satisfied by the executed entry `pool_script_v2::swap_b2a`,
whose signature is `(&GlobalConfig, &mut Pool<T0, T1>, Coin<T0>, Coin<T1>, bool, u64, u64, u128,
&Clock, &mut TxContext)` — a mutable borrow of the pool's generic state with coin legs on both of its
type parameters. Clause (2) holds concretely: across transaction T `current_sqrt_price` rose from
`647_308_812_393_509_050_120` to `647_324_162_169_833_037_484` while `coin_b` rose and `coin_a`
fell, and none of the 9 input objects is a price feed. Clause (3): the pool's type and its package
link no oracle. Every unit of price in this object is discovered by trading against it.

### Object B — `0x549e8b69…::native_pool::NativePool` (Volo liquid staking): **no**

It is pool-shaped — `pending` and `collectable_fee` coins, a `vaults` table, a
`validators` map — and it does hold SUI. But clause (1) fails **structurally and unconditionally**:
`NativePool` has no type parameters, so no function on it can borrow generic state and exchange two
of its own assets. Its 77 functions were scanned; the coin-touching ones are `stake` (SUI in, no
coin out), `unstake`/`mint_ticket` (CERT in), and `burn_ticket` for the ticket —
each moves *one* asset against a share claim. The SUI↔VSUI rate is an accounting ratio,
`total_staked / total_shares`, that moves when rewards accrue or validators are rebalanced, never
when somebody trades. Clause (2) fails too: the object holds no price variable at all. Two coin
fields and a ratio is a vault, not a venue.

### Object C — `0xd899cf7d…::storage::Storage` (Navi lending): **no**

Its entire BCS is 155 bytes and contains **no balances**: `reserves` and `user_info` are
`0x2::table::Table`s, so the object carries two `UID`s, two lengths and a version, while the 35
reserves and ~999k user positions live in dynamic fields. Clause (1) fails for the same structural
reason as B — `Storage` has no type parameters, so `deposit`/`withdraw`/`borrow`/`repay`
each move one asset against a share claim and none of them can exchange two of the object's own
assets. Clause (2) fails: nothing in those 155 bytes is a price. Clause (3) **fails as well**: the
package statically links `oracle::PriceOracle`, reached from `calculator`, `dynamic_calculator`,
`lending` and `logic`. Asset values are imported and interest is a utilisation curve; `Storage` is a
ledger, and a ledger with a price feed attached is still not a place where price is discovered.

---

## 3. Recreate state and reproduce T

`cargo run -- reproduce` prints the whole derivation. The short version:

```
pre-state  pool A @ version 995_150_484   (reached two ways: by version, and as the object in
                                           checkpoint 320_577_815's deduplicated object set)
  liquidity        L   120_115_891_674_982
  current_sqrt_price S 647_308_812_393_509_050_120      (= 35.090681033302715 × 2^64)
  current_tick_index   71_162          fee_rate 500 (5 bps)      tick_spacing 10

step
  fee                  50_000_000      (100e9 × 500 / 1e6 — matches the stated fee exactly)
  amount_in_after_fee  99_950_000_000
  ΔS = ⌊in · 2^64 / L⌋ 15_349_776_323_987_364
  S' = S + ΔS          647_324_162_169_833_037_484

result
  amount_out           81_168_759      ← the chain's value
  difference           0
  steps                1               liquidity unchanged: 120_115_891_674_982
```

**Liquidity, price and range in force.** At the version T consumed, the active tick range is
`[71_060, 71_190)`, with `L = 120_115_891_674_982`; the next initialised tick above is `71_190` at
`sqrt_price = 648_206_889_171_250_166_865`. (Both live and fixture runs print this bracketing,
because the tick set can only ever be read at its current version: the `71_180` tick seen at
research time has since been burned, which is exactly why the quote is proven without the tick
set — see the spacing argument below.)
The step reaches `647_324_162_169_833_037_484`, which is
below that, so **no tick is crossed and `L` is constant** — one step, exactly as the transaction
description says. The move is 0.474 ticks, against a `tick_spacing` of 10.

That last fact is why the quote is exact *without the tick set*: a price move smaller than
`tick_spacing` cannot reach another initialised tick, because initialised ticks sit on the spacing
grid. The command exploits it — it quotes both with no tick boundaries at all and with the live tick
children, and asserts the two agree — which matters because a pool's tick set can only be read at its
current version, never at a historical one.

**Rounding, and the difference from the chain.** There is none: the recomputation equals
`81_168_759` exactly, and it is a test, not a printout
(`birdai-amm::swap::tests::output_matches_the_chain_exactly`). The directions that produce it:

| Step | Direction | Why |
|---|---|---|
| `fee = ⌊in · fee_rate / 1e6⌋` | floor | protocol constant |
| `ΔS = ⌊in · 2^64 / L⌋` | floor | smallest price improvement the input pays for — pool-favourable |
| `out = ⌊(L ≪ 64) · ΔS / (S · S')⌋` | floor | pool-favourable; matches the V3-lineage `getAmount0Delta(round_up = false)` |

Rounding `ΔS` **up** instead still yields `81_168_759`, because a one-unit change in `ΔS` moves the
output by far less than one base unit at this step width. The reproduction is exact *and*
insensitive, which is a stronger statement than a lucky match. A floating-point path would give
`81_168_759.00 ± 1` with an unreliable last digit, which is why the implementation is integer-only.

Two implementation notes that decide correctness:

* **`U256` is provably sufficient — no 512-bit arithmetic is needed.** Each `Balance` is a `u64`, so
  virtual reserves are `< 2^64` and `L = √(a_v·b_v) < 2^64`; therefore `L ≪ 64 < 2^128` and the
  numerator `(L ≪ 64)·ΔS < 2^256`, while `S·S' < 2^256` too. Arithmetic goes through a `CheckedU256`
  wrapper because `move_core_types::u256::U256`'s `Add`/`Sub`/`Mul` **wrap** and its `Div` panics on
  a zero divisor — wrapping in a pricing path is a silent wrong answer.
* **Tick prices are read, never recomputed.** Validating the captured set's 653 nodes against
  `⌊1.0001^(t/2)·2^64⌋` shows 651 exact and a worst case 7 units low at `√P ≈ 7.9·10^28`, a relative
  error under `2^-90` (deviation histogram `{-7: 1, -1: 1, 0: 651}`). Exhaustive search over the
  plausible shapes of the on-chain routine found no
  variant that reproduces every observed tick, so the stored values are authoritative and the
  function is used only for the inverse mapping and for validating that a node is what it claims.

---

## 4. Design note — keeping state current

State is kept current by a `StateManager` fed from
`sui_indexer_alt_framework::ingestion`, which supplies hybrid gRPC streaming plus object-store
backfill, retries, backoff, adaptive concurrency and per-subscriber backpressure without a database.
The boundary is deliberately three values: **BCS bytes, `StructTag`, version**. Everything above it —
`Arc<VenueSlot>` keyed by object id in an `scc::HashMap` — knows nothing about RPC, checkpoints or
encoding; everything below it is `sui_types::object::Object`, the same type the stream and a
validator both produce.

Three problems dominate. *New pools* are found by `module::name` and then **confirmed by the resolved
layout's fields**, because several mainnet packages define `pool::Pool` with different layouts; the
name is a hint, the shape is the test. *Dynamic-field churn* is harder than it looks: tick nodes hang
off the skip list's **inner UID**, not the pool, so they are indexed by the owning UID and bounded per
parent. The skip list's declared `size` is an assertion, and it legitimately disagrees — children can
only be listed at their current version — so the skew is reported rather than hidden. *Package
upgrades* need the cache keyed by layout **dependencies**: each cached layout records every package
it mentions, and a package seen in a checkpoint is recorded under its **original** package id,
because an upgrade creates a new object id while layouts canonicalise to the original.

Inside a validator the boundary is unchanged but its properties invert: state is provisional, so the
manager needs two-phase commit and rollback; layouts come synchronously from `ModuleCache` and
upgrades are visible immediately; BCS is already trusted and can be borrowed without copying. Only
the source and the commit protocol change — no venue, tick or math code does.

*(Word count of the note proper: ≈275, limit 300.)*

---

## What the on-chain data changed about the design

Every one of these was found by running against mainnet, and each contradicts something that looked
obvious beforehand. They are listed because the reasoning matters more than the numbers.

1. **Cetus's `pool` module contains no swap.** 77 functions scanned, none with the inter-asset shape.
   The entry that moved pool A is in a sibling package, `pool_script_v2::swap_b2a`. A probe that
   only looked at the defining package would report a false negative on the clearest venue in the
   set — which is why the probe also resolves the entry the chain actually executed.
2. **`module::name` is not identity.** `follow` immediately hit 27 objects named `pool::Pool` from
   other packages whose layouts differ, failing the shape check on every one. Name is now a hint
   and the resolved layout's field set is the test; name collisions are counted as `unrecognised`,
   not `failed`.
3. **A pool's children cannot be read at a historical version.** Transaction T consumed pool A at
   version 995 150 484, which declares 650 ticks; enumerating the nodes at capture time returned
   653, because dynamic fields can only be listed as of the present (and the live count keeps
   drifting as the pool trades). The skew is reported, not swallowed.
4. **On-chain tick prices are not exactly `⌊1.0001^(t/2)·2^64⌋`.** Two of the captured 653 nodes differ,
   one by 7 units. No plausible variant of the decomposition reproduces every observed value, so the
   stored per-tick prices are treated as authoritative — which the swap math already did.
5. **The `hd` suffix of the fixed-point format is derivable, not memorable.** `S / 1.0001^(tick/2)`
   lands within one tick of `2^64`, which is what `calibrate` asserts rather than hard-coding.
6. **`sui-indexer-alt-framework`'s `default-features = false` does *not* remove Diesel/Postgres.**
   `sui-indexer-alt-metrics` depends on `sui-pg-db` unconditionally. The framework is still the right
   choice because nothing else ships the ingestion stack, but the claim had to be corrected.
7. **`move_core_types::u256::U256`'s operators wrap** and `Div` panics on zero. Nothing in its
   documentation index says so; its own source does, and a pricing path cannot use them.
8. **`Client::batch_get_objects` collapses one missing object into a wholesale failure.** Enumerating
   a pool's ~650 tick nodes and then fetching them takes long enough that a tick can be removed in
   between — which is exactly what happened, taking the whole capture down. The source now retries
   with bounded concurrency (32 at a time) and skips what is gone; a batch that answers short is
   re-fetched the same way rather than trusted positionally.
9. **Loading a fixture directory that does not exist must fail.** The first version silently returned
   an empty set, so a mistyped `--fixtures` path surfaced as "object not found" three commands later.
10. **GetBlock's Sui endpoint does not serve gRPC v2.** `shared.eu-central-1.getblock.io/<key>` answered
   every request with `Missing token-id`, with the key in the path and in each of `x-api-key`,
   `x-token-id` and `Authorization: Bearer`. `--api-key` is supported anyway, since providers that do
   offer gRPC expect the header; the capture above came from the public `fullnode.mainnet.sui.io`.
11. **Sui's two public mainnet endpoints are not interchangeable, so the run talks to both.**
    `fullnode.mainnet.sui.io` serves the whole API but keeps only a bounded window of history;
    `archive.mainnet.sui.io` keeps the full history but does **not** implement `StateService` —
    `ListDynamicFields` answers `Unimplemented` there. Transaction T is in checkpoint 320 577 815 and
    consumes pool A at version 995 150 484, both outside a fullnode's retention, so checkpoints *and
    versioned object reads* are routed to the archival endpoint while latest reads and dynamic
    fields stay on the fullnode (`--archive-url`, defaulting to `archive.mainnet.sui.io`, with `""`
    opting out). This also explains the transient `unavailable` errors seen before the split — and
    the `object … not found at version 995150484` failure that forced the versioned-read half of it.
12. **One bad object must not kill a checkpoint.** Replaying the captured checkpoint through the
   state manager surfaced two cases the happy path never meets: the same pool mutated twice in one
   checkpoint (so a naive re-apply walks versions backwards), and a second Cetus deployment whose
   package the capture predates (so its layout does not resolve). Checkpoints are now deduplicated
   by sequence with per-object version monotonicity behind them, and an unresolvable tag skips its
   objects with a counted failure instead of aborting the apply.
13. **A swap's price limit needs a side.** `swap_exact_in` accepted any `price_limit`, so a limit
   behind the price walked the price backwards and a stale tick source could do the same through a
   boundary. Both are now rejected or ignored up front (`UnreachablePriceLimit`), with a test on
   each side — which is also what finally constructs that error variant.
14. **Decoders fail closed on untrusted lengths.** A corrupt BCS length prefix could drive a huge
   `Vec` pre-allocation, and a layout without Cetus's `OptionU64.v` decoded as `Some(0)`. The walk
   still reads every element the driver yields, but the reservation is capped — and a missing
   payload is `MissingField`, never a zero.

---

## Repository layout

```
bin/birdai                 CLI: decode | classify | reproduce | calibrate | follow | fetch
crates/birdai-move         protocol newtypes (I32, I128, OptionU64), the decoder toolkit, the dump
crates/birdai-resolve      gRPC object source and a layout cache that invalidates on package upgrade
crates/birdai-amm          tick math, delta math, exact-input swap, CheckedU256   (no I/O)
crates/birdai-tick         the Cetus tick skip list: decode, index, validate, bracket
crates/birdai-venue        typed venues and the price-discovery classifier
crates/birdai-state        checkpoint-driven in-memory venue state
docs/design.md             the full design, including the 33 defects found across two reviews
```

`birdai-amm` and `birdai-tick` have no network dependency and are exercised entirely by unit and
property tests. `just test` runs everything; `just lint` runs the pedantic Clippy set; `just
deps-check` verifies the Sui pin.

## Verification

```
$ cargo test --all-features
  birdai (bin)      5 passed   # incl. B/C decoding offline as vault and ledger
  birdai-amm       50 passed   # incl. output_matches_the_chain_exactly (Δ = 0)
  birdai-move       6 passed
  birdai-resolve   17 passed
  birdai-state      5 passed   # incl. offline replay of checkpoint 320577815
  birdai-tick      35 passed
  birdai-venue      7 passed
  birdai-amm doctest 1 passed
  ─────────────────────────
  126 passed, 0 failed

$ cargo run -- --fixtures fixtures reproduce   # 81_168_759 out, difference 0, ✔ exact match
$ just lint        # typos, rumdl, cargo-sort, nightly fmt --check, nightly clippy -D warnings,
                   # cargo-shear, workspace-inheritance-check — all green
$ just deps-check  # all git dependencies pinned at f0831497...
```

Mutation testing: a scoped `cargo mutants` run over `birdai-tick/src/index.rs` (where the
pricing invariants live) reports 103 caught, 33 unviable, **0 missed** — see `mutants.out/`.
The guards added since that run (wrong-side price limits, stale boundaries, score/key
consistency, version monotonicity, fail-closed `OptionU64`) each carry a killer test, verified by
reverting the guard by hand and watching exactly its test fail.

## Status

Everything in the brief is implemented and verified against mainnet: decode (including a tick child),
classify, reproduce (`Δ = 0`), and the state manager described in the design note, which runs against
the live checkpoint stream and loads a freshly seen pool's ticks on first sight. 126 tests and a
pedantic Clippy pass with `-D warnings`. Known limitations, all deliberate:

* The layout cache's upgrade invalidation is exercised by unit tests on the dependency graph, not yet
  by a live package upgrade observed in the stream (none occurred during testing).
* The captured checkpoint is filtered to the transactions touching pool A, so a full replay of an
  unrelated checkpoint would need a wider capture. `fetch` takes the filter as a closure for exactly
  that reason.
