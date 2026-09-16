# Default recipe to display help
default:
  @just --list

# Format all code
format:
  rumdl fmt .
  cargo sort -w -g
  cargo +nightly fmt --all

# Auto-fix linting issues
fix:
  rumdl check --fix .
  RUSTC_WRAPPER= cargo +nightly clippy --fix --all --all-targets --allow-dirty
  cargo workspace-inheritance-check --fix

# Run all lints
lint:
  typos
  rumdl check .
  cargo sort -w -g -c
  cargo +nightly fmt --all -- --check
  RUSTC_WRAPPER= cargo +nightly clippy --all --all-targets -- -D warnings
  cargo shear
  cargo workspace-inheritance-check

# Run tests
test:
  cargo test --all-features

# Verify every git dependency is pinned by `rev` (not `branch`) and all revs are identical
deps-check:
  #!/usr/bin/env bash
  set -euo pipefail
  revs=$(rg -o --no-filename 'rev = "[0-9a-f]+"' Cargo.toml crates/*/Cargo.toml bin/*/Cargo.toml | sort -u)
  branches=$(rg --no-filename 'branch = "' Cargo.toml crates/*/Cargo.toml bin/*/Cargo.toml || true)
  if [ -n "$branches" ]; then echo "branch-pinned dependencies found:"; echo "$branches"; exit 1; fi
  count=$(echo "$revs" | wc -l | tr -d ' ')
  if [ "$count" -ne 1 ]; then echo "git dependencies pin different revs:"; echo "$revs"; exit 1; fi
  echo "all git dependencies pinned at $revs"

# Run mutation tests with cargo-mutants
mutation:
  cargo mutants

# Run tests with coverage
test-coverage:
  cargo tarpaulin --all-features --workspace --timeout 300

# Build entire workspace
build:
  cargo build --workspace

# Check all targets compile
check:
  cargo check --all-targets --all-features

# Publish all crates to crates.io (dry run)
publish-check:
  cargo publish --workspace --dry-run --allow-dirty

# Publish all crates to crates.io
publish:
  cargo publish --workspace

# Check for Chinese characters
check-cn:
  rg --line-number --column "\p{Han}"

# Full CI check
ci: lint test build

# ============================================================
# Maintenance & Tools
# ============================================================

# Clean build artifacts
clean:
  cargo clean

# Install all required development tools
setup:
  cargo install cargo-mutants
  cargo install cargo-shear
  cargo install cargo-sort
  cargo install cargo-workspace-inheritance-check
  cargo install typos-cli
  cargo install rumdl

# Generate documentation for the workspace
docs:
  cargo doc --no-deps --open
