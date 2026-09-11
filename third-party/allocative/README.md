# `third-party/allocative`

A vendored copy of [`allocative 0.3.4`](https://crates.io/crates/allocative/0.3.4)
(Facebook/Meta, MIT/Apache-2.0 — licenses preserved in `LICENSE-MIT`/`LICENSE-APACHE`),
used through a `[patch.crates-io]` entry in the workspace root `Cargo.toml`.

## Why it exists

Two constraints collide on this one crate:

1. The Move package system (`starlark_map 0.13.0`, via Sui) needs allocative's `Allocative`
   impls to resolve against `hashbrown 0.14`. `allocative >= 0.3.6` moved to hashbrown 0.16
   and no longer compiles against it, so the version must stay at `0.3.x` with
   `hashbrown ^0.14.3` — and `Cargo.lock` pins exactly `0.3.4`.
2. `allocative 0.3.4` (and `0.3.5`) do not compile on recent nightlies: the
   `#[cfg(rust_nightly)] impl Allocative for !` in `src/impls/std/unsorted.rs` conflicts
   (E0119) with `impl Allocative for Infallible`, which newer compilers consider
   overlapping. The workspace lint recipe (`just lint`) and CI run clippy on a floating
   nightly, so this third-party failure fails our build.

## What was changed

Exactly one hunk: the redundant `!` impl was deleted (see the `NOTE(birdai)` comment in
`src/impls/std/unsorted.rs`). Stable toolchains never compiled that impl — the crate's own
`build.rs` only sets `rust_nightly` when `rustc --version` says nightly — so the patched
crate behaves identically to upstream on stable, and additionally compiles on nightly.

## When to remove this directory

If upstream releases an `allocative` that both keeps `hashbrown 0.14` compatibility and
compiles on current nightly, or once `starlark_map` moves off `hashbrown 0.14`, delete
this directory and the `[patch.crates-io]` entry and re-pin in `Cargo.lock`.
