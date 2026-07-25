# Rust Developer Style Guide

This guide establishes coding standards, conventions, and workflows for developers contributing to
**infrastore**. Following these guidelines keeps the codebase consistent across its five crates and
four language bindings, and streamlines review.

## Pre-commit Checks

The repository uses `cargo-husky` to install a pre-commit hook. The hook runs:

```bash
cargo fmt --all -- --check                              # Rust formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings
dprint check                                             # Markdown formatting
```

The hook is installed when `infrastore-core` is built. If any check fails, the commit is blocked. It
also runs `shellcheck` when that tool is available. Run `cargo test --workspace --all-features`
before committing and keep the workspace clippy-clean.

## Code Formatting

### Rust Formatting (rustfmt)

All Rust code must pass `cargo fmt --all -- --check`. Run `cargo fmt --all` before committing to
auto-format.

**Key conventions enforced:**

- 4-space indentation
- Max line width of 100 characters
- Consistent brace placement
- Sorted imports

### Clippy Compliance

All code must compile without clippy warnings when run with `-D warnings`:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The workspace targets Rust edition 2024 (requires Rust 1.95+). Match the idioms of the surrounding
code.

**Common clippy lints to watch for:**

- `clippy::unwrap_used` — prefer `expect()` with a descriptive message, or proper error handling
- `clippy::clone_on_copy` — avoid cloning `Copy` types
- `clippy::needless_return` — omit unnecessary `return` keywords
- `clippy::redundant_closure` — use method references where possible

## Workspace Layout

The workspace is a single core crate plus four binding crates that wrap it. Keep that dependency
direction: bindings depend on `infrastore-core`, never the reverse.

| Crate               | Role                                                     |
| ------------------- | -------------------------------------------------------- |
| `infrastore-core`   | Types, NetCDF + SQLite storage, hashing, public Rust API |
| `infrastore-proto`  | Protobuf service definition, tonic codegen, conversions  |
| `infrastore-server` | gRPC server binary + Rust client                         |
| `infrastore-py`     | PyO3 bindings (abi3-py310 wheel)                         |
| `infrastore-ffi`    | C ABI cdylib consumed by the Julia binding               |

Shared dependency versions are declared once in the root `Cargo.toml` `[workspace.dependencies]`.
Reference them from member crates with `{ workspace = true }` rather than pinning a version locally.

## Feature Implementation Across Bindings

The defining constraint of this repo: a single core feature is exposed through four bindings. When
you add or change something in the core public API, propagate it through every binding before
considering the work done.

| Surface       | Location                                                              | Notes                                          |
| ------------- | --------------------------------------------------------------------- | ---------------------------------------------- |
| Core Rust API | `infrastore-core/src/store.rs`                                        | The source of truth                            |
| gRPC          | `proto/`, `infrastore-proto/src/`, `infrastore-server/src/service.rs` | Read-only server; writes need local filesystem |
| Rust client   | `infrastore-server/src/client.rs`                                     | Mirrors the gRPC surface                       |
| Python        | `infrastore-py/src/lib.rs`                                            | PyO3; keep `python/tests/` in sync             |
| Julia / FFI   | `infrastore-ffi/src/lib.rs`, `julia/TimeSeries.jl/src/`               | C ABI; regenerate the header (below)           |

### Proto / gRPC changes

1. Edit the `.proto` source under `proto/`.
2. The proto crate's build script regenerates the tonic code; add hand-written conversions in
   `infrastore-proto/src/convert.rs`.
3. Update the server (`service.rs`) and Rust client (`client.rs`) to cover the new surface.
4. Run `cargo test -p infrastore-server` (includes `tests/grpc_round_trip.rs`).

### FFI / Julia changes

The `extern "C"` surface in `infrastore-ffi/src/lib.rs` is the contract for the Julia binding. After
changing it:

1. Regenerate `include/infrastore.h` via `cbindgen` and keep the checked-in header in sync.
2. Update the Julia wrapper in `julia/TimeSeries.jl/src/` and its tests.
3. Build with `cargo build -p infrastore-ffi --release` and run the Julia test suite.

Cross-language types must round-trip. Add a case to the relevant round-trip test
(`tests/round_trip.rs`, `tests/netcdf_roundtrip.rs`, `python/tests/`, `julia/.../test/`).

## Error Handling

### Library code

Use typed errors with `thiserror`. The core crate defines the shared error type and `Result` alias —
add variants there rather than introducing parallel error enums:

```rust
// infrastore-core/src/error.rs
use thiserror::Error;

pub type Result<T> = std::result::Result<T, TimeSeriesError>;

#[derive(Debug, Error)]
pub enum TimeSeriesError {
    #[error("time series not found")]
    NotFound,
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    // ...
}
```

- Use `#[from]` for foreign errors that map cleanly (`std::io::Error`, `rusqlite::Error`,
  `serde_json::Error`).
- **Reserved-but-unimplemented behavior** (e.g. the five time-series types beyond
  `SingleTimeSeries`, or multi-dim per-step values in the NetCDF backend) must return
  `InvalidParameter`, never silently mis-handle input. This preserves the v0 forward-compatibility
  contract.

### Test code

Use `.expect()` with descriptive messages instead of `.unwrap()`:

```rust
let store = Store::create_in_memory().expect("in-memory store should initialize");
```

## Testing

Tests use the built-in test harness — `#[test]` for synchronous code and `#[tokio::test]` for the
async server paths. There is no `rstest`/`anyhow` dependency; don't add one without a reason.

### Organization

- **Unit tests**: inline `#[cfg(test)]` modules next to the code (e.g. `hash.rs`, `auth.rs`).
- **Integration tests**: under each crate's `tests/` directory:
  - `infrastore-core/tests/round_trip.rs`, `netcdf_roundtrip.rs`
  - `infrastore-server/tests/grpc_round_trip.rs`, `auth.rs`
- **Python**: `python/tests/` (pytest) — run via `maturin develop` then `pytest`.
- **Julia**: `julia/TimeSeries.jl/test/runtests.jl`.

### Guidelines

1. Cover error conditions, not just the happy path — especially the `InvalidParameter` rejections
   that enforce v0 scope.
2. Keep tests focused: one behavior per test function.
3. Prefer in-memory stores for fast unit tests; reserve NetCDF/SQLite file tests for the storage and
   round-trip suites.
4. New cross-binding features need a round-trip test in each affected binding's suite.

## Logging

Use `tracing` for structured logging (currently in the server binary). Prefer structured fields over
interpolated strings:

```rust
use tracing::{info, warn};

info!(owner_id, name, "added time series");
```

Enable debug logging with:

```bash
RUST_LOG=debug cargo run -p infrastore-server -- --config my_server.toml
RUST_LOG=infrastore_server=debug cargo run -p infrastore-server  # fine-grained
```

## Configuration Priority

For the server, CLI arguments override config-file values. Resolve in that order and document any
new option in `examples/server.toml`.

## Summary Checklist

Before submitting a pull request, verify:

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] New error cases use `TimeSeriesError` variants; out-of-scope input returns `InvalidParameter`
- [ ] Core API changes are propagated through proto/gRPC, Rust client, Python, and FFI/Julia
- [ ] The cbindgen header `infrastore.h` is regenerated if the FFI surface changed
- [ ] Cross-binding features have round-trip tests in each affected binding's suite
- [ ] `examples/server.toml` updated if server configuration changed
