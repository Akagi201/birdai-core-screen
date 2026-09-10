# Birdai Core Screen: decode, classify, recreate

Birdai Labs builds MEV and execution-quality infrastructure on Sui. Birdai Core is the layer that finds every trading venue on chain, decodes its objects, and recreates pool state exactly so we can price against it in memory. This exercise is a small, real slice of that work. It is time-boxed to four hours. Use any tools, libraries, or AI assistants you like; we are grading how you reason about Sui objects and whether the numbers come out right, not whether you already knew the layouts. Code parts must be in Rust.

## Inputs (Sui mainnet, all shared objects)

- **Object A**: `0x51e883ba7c0b566a26cbc8a94cd33eb0abd418a77cc1e60ad22fd9b1f29cd2ab`
  Type: `0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb::pool::Pool<USDC, SUI>` (Cetus CLMM)
- **Object B**: `0x7fa2faa111b8c65bea48a23049bfd81ca8f971a262d981dcd9a17c3825cb5baf`
  Type: `0x549e8b69270defbfafd4f94e17ec44cdbdd99820b33bda2278dea3b9a32d3f55::native_pool::NativePool` (Volo liquid staking)
- **Object C**: `0xbb4e2f4b6205c2e2a2db47aeb4f830796ec7c005f88537ee775986639bc442fe`
  Type: `0xd899cf7d2b5db716bd2cf55599fb0d5ee38a3061e7b6bb6eebf73fa5bc4c81ca::storage::Storage` (Navi lending)

**Transaction T**: `F53RBSPn84e28FDWnunb7dykGTp7sNpzEnNUxG5h5fe7`, checkpoint `320577815`
A single swap on Object A: 100 SUI in (`100,000,000,000 MIST`, B to A), `81,168,759` USDC base units out, fee `50,000,000 MIST`, one step.

## Where to get layouts and bytes

The GraphQL endpoint returns an object's raw BCS and its type layout in one query:
```graphql
object(address) {
  asMoveObject {
    contents {
      bcs
      type { layout }
    }
  }
}
```
The `sui-package-resolver` crate in the Sui repo resolves any type tag to a `MoveTypeLayout` from on-chain package bytecode.

Checkpoint data (`sui-data-ingestion-core`, or the gRPC checkpoint stream) carries every input and output object as `sui_types::object::Object`, whose Move contents are the same BCS bytes.
> Do not use the node's pre-parsed JSON as your decoder; it is fine for checking your answers.

## Tasks

### 1. Decode

For A, B, and C, fetch the BCS contents and deserialize them in Rust into typed values using a resolved layout.
For A, also decode one dynamic-field child: a tick entry from `tick_manager.ticks` (a skip list) at or near the current tick.

Output a field-by-field dump of each object and the code that produced it.
Note where generics, `Option`, `Balance`, and Table or Bag children required special handling.

### 2. Classify

Which of A, B, and C are trading venues with on-chain price discovery?
Write one paragraph per object. State the test you applied.

> A test based on field names or the presence of balance fields will not score well; we are interested in what actually distinguishes a pool where price is discovered on chain from an object that merely holds balances and looks pool-shaped.

### 3. Recreate state and reproduce T

Using Object A's state immediately before transaction T (the version of the pool that T took as input), compute the output amount for T's input, fees included, and compare it with the on-chain result.

Show the liquidity, sqrt price, and tick range in force for that step, and explain any difference between your number and the chain's, including rounding direction.

### 4. Design note (300 words or fewer)

How would you keep this state current in memory from the checkpoint stream at checkpoint cadence, handling new pools appearing, dynamic-field churn, and package upgrades that change layouts?

Describe the boundary in your code between the checkpoint object and the typed state, and what would have to change if the objects arrived from inside a validator process rather than from the stream.

## Deliverable

A repository link with a `cargo run` that reproduces the numbers, and a README containing your answers to 2, 3, and 4.
