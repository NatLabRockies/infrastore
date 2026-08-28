# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## Project Overview

**infrastore** is a Rust library for managing time-series data in power-systems / energy
simulations. Persistence is split between numerical arrays in HDF5 and metadata associations in
SQLite. It exposes multiple bindings over a shared core:

- **Native Rust** — `infrastore-core` public API
- **gRPC server + Rust client** — `infrastore-server` (read-only server; writes need local
  filesystem access)
- **Python** — `infrastore-py` via PyO3 (abi3-py311 wheel)
- **Julia** — `infrastore-ffi` C ABI cdylib, wrapped by `julia/InfraStore.jl`
- **CLI** — `infrastore-cli` (`infrastore` binary): loads time series from CSV + a descriptor JSON
  and inspects a store, talking directly to the on-disk HDF5 + SQLite artifact (read+write; no
  gRPC). Output uses a global `-f/--format table|json|jsonl|csv`.

**Current feature coverage:** `SingleTimeSeries` and `NonSequentialTimeSeries` are implemented
end-to-end (read+write in the Rust core, C ABI, Python, and Julia; read-only over gRPC).
`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` support reading
values across the Rust core, C ABI, Python, Julia, and gRPC. Dense forecasts (`Deterministic`,
`Probabilistic`, `Scenarios`) are written through the generic `add_time_series` by passing the
matching forecast object across the Rust core, Python, and Julia (the C ABI keeps per-type
`infrastore_store_add_forecast` / `infrastore_store_add_probabilistic` as low-level transport);
`DeterministicSingleTimeSeries` is derived from stored `SingleTimeSeries` via
`transform_single_time_series` rather than added directly. Forecast writes are not exposed over the
read-only gRPC server. Arrays are dtype-generic (`f64`/`f32`/`i64`/`i32`/`u64`/`bool` in every
binding, including Python) and may have multidimensional per-timestep values. The columnar
simulation readers (`StaticReader`/`ForecastReader`) are bound across the Rust core, C ABI, Julia,
and Python; `StaticReader` covers both static types, sweeping either a `SingleTimeSeries` grid or a
cohort of `NonSequentialTimeSeries` sharing one timestamp vector (its `resolution()` is `None` for
the latter). The discovery/maintenance surface (`get_intervals`, `list_names`, `list_owner_types`,
name-pattern filtering via `ListFilter::name_glob` (SQLite `GLOB`), `ListFilter::component_field`
(exact match; served by the partial index `idx_component_field`, so it can never select rows that
left the field unset), `remove_by_filter`, `remove_time_series_bulk`, `rename_time_series`,
time-sliced `bulk_read`, `AddRequest`/`Store::add` preserving `application_data`, and serde on the
core types) is available in the Rust core and threaded through the C ABI/Julia and Python bindings.
Two **association catalogs** are available in the Rust core, C ABI, Julia, Python, and the CLI (read
via `attributes` / `links`, write via `attach` / `detach` / `link` / `unlink` / `reassign`), but not
over gRPC: `supplemental_attribute_associations` (component ↔ supplemental attribute, the wider
surface — counts, counts-by-type, grouped summary) and `parent_child_associations` (directed
component ↔ component edges, e.g. a generator connected to a bus, deliberately narrower until a
consumer needs more). Both are independent of time series in both directions, and of each other.
Every catalog row also carries an **`id`** — an `INTEGER PRIMARY KEY AUTOINCREMENT`, so it is never
reissued once its row is deleted — which a consumer stores in its own object model to reference a
series later (a generator's `operation_cost` naming the series that varies it). Writes return it
(`AddedTimeSeries` in the Rust core, Python, and Julia; an `out_id` across the C ABI), reads resolve
it (`get_metadata_by_id`, `association_exists`, `read_by_ids`), and it crosses the gRPC wire and the
OpenAPI one — where the schema spells it `association_id`, a rename `openapi.rs` applies the same
way it maps `unit_system` between the store's snake_case and the schema's SCREAMING_CASE. It is
descriptive — outside `TimeSeriesKey` and both content hashes — but unlike the descriptors above it
describes the _row_ rather than the data: it is per-store, so `merge` assigns fresh ids and `diff`
ignores it, while `rename`/`reassign`/`compact`/`persist_to` all preserve it. **No `add_*` accepts
an id** — not `add_time_series`, a bulk add, or either association catalog's attach/link — because
"never reissued" is a guarantee of `AUTOINCREMENT`, and a caller free to name an id could re-file a
retired one. The single exception is the rows-only import `import_time_series_associations_openapi`
(`Store::import_association_rows`), which files each row under the `association_id` the document
recorded so its references survive: all-or-none across the batch, and only above the catalog's
high-water mark. It refuses a row whose array is absent, a `DeterministicSingleTimeSeries` whose
source `SingleTimeSeries` is neither in the document nor already stored, and
`NonSequentialTimeSeries` outright (its timestamp vector is not on the wire). Neither association
catalog's wire form carries an id, so both always assign — their row types carry an `id` field that
a listing populates and an add ignores — with independent counters, and equality on both association
types deliberately excludes the id.

Metadata getters surface `element_shape` and `features` in every binding. Alongside `units`, a
series carries two further unit descriptors in every binding: `quantity_kind` (free-form, QUDT
`QuantityKind` local names recommended — it separates active from reactive power, which dimensional
analysis cannot, and is the only record of what per-unit values measure) and `unit_system`
(`natural_units` | `component_base`, a label the store never acts on; unset means _unspecified_, not
natural units). A series also carries `component_field` (free-form; names the field on the owning
component whose value these values are the time-varying form of, e.g. `max_active_power` — it
records what the values are _for_, where `name` only says which series they are; it is the one
descriptor that is also a filter, in every binding). All three are descriptive, so they sit outside
`TimeSeriesKey` and outside both content hashes, alongside `application_data` — the opaque
package-owned payload formerly spelled `ext`.

Every series also carries a **`time_reference`** (`TimeReference`: `Utc` | `FixedOffset(minutes)` |
`Zone(iana_name)` | `Zoneless`; `None` means unspecified), recording how its timestamps were
_spelled_ so a read hands back what a write declared instead of relabelling everything UTC. Each
binding **infers** it from the input type — Python from `tzinfo` (naive → `Zoneless`, a `key`-
bearing `ZoneInfo` → `Zone`), Julia from `DateTime` vs `ZonedDateTime` (`FixedTimeZone` vs
`VariableTimeZone` in `InfraStoreTimeZonesExt`), the CLI from the text plus `--assume-timezone` /
`--zoneless`; a native Rust caller declares it. It is descriptive like the three above (outside the
key and both hashes, so two series differing only in it are a duplicate), but it is _not_ inert:
query bounds must match the series' spelling (`TimeRange` carries a `zoneless` flag and the core
refuses a mismatch rather than coercing), and a selection spanning both coherence groups is refused
by `bulk_read_range` and `build_static_reader`, with `ListFilter::zoneless`
(`--spelling zoned|zoneless` in the CLI) as the constructive remedy. A reference is a **spelling,
not a grid**: `Period::Months` still steps on the UTC calendar (warned about when it meets a zoned
reference), and a local-clock grid belongs in `NonSequentialTimeSeries`. The core validates a zone
name's _shape_ only and never resolves it — no tz database; existence is audited by the layers that
have one (the CLI via `chrono-tz`, Python via `zoneinfo`, Julia via `TimeZones`) and reported by
`store-info`. The CLI is the one place that runs local → instant, so `--assume-timezone <IANA name>`
refuses the skipped and repeated wall clocks per row rather than guessing. Python ships type stubs
(`infrastore.pyi` + a pytest drift guard), a full exception hierarchy, and keyword-only optional
arguments; Julia returns its catalog/metadata/summary query results as structs
(`TimeSeriesMetadata`, `KeyRow`, `StaticGrid`, … — see
`docs/src/reference/julia-api.md#result-types`), overloads `Base`
(`==`/`hash`/`show`/`length`/`iterate`), and offers do-block `Store`/`open_store` forms. A stored
`DeterministicSingleTimeSeries` always reads back as a `Deterministic` (storage-level view, by
design); the DST tag remains visible in catalog surfaces (keys, metadata, counts). The CLI
additionally has `export` (bulk read-direction inverse of `add`; its timestamped CSV is re-readable
by `add`, which detects the layout from the header), `arrays` / `store-info` and the `data_hash` +
resolved HDF5 dataset/column on `list`/`info`, `--name-glob` selectors, `--dry-run` on destructive
commands, store-creation `--compression` flags, shell `completions`, and a `INFRASTORE_STORE` env
fallback. It also carries a **wide-CSV ingest** (`"layout": "wide"` plus an
`owner_map`/`owner_id_from` column→owner mapping) and its inverse `grid`, which drives the core's
`StaticReader`; discovery commands (`names`, `owner-types`, `owners`, `exists`); charting
(`get
--plot` sparklines and `plot --kind line|duration|heatmap|fan|overlay`, rendered by the
hand-written `src/chart/` SVG backend — deliberately no charting dependency, because `deny.toml`
makes one a policy decision); `diff` and `merge` between two stores; `init` and
`--catalog attached|in-memory`; and an inline flag form of `add` alongside `--descriptor -` (stdin),
`--dry-run`, `--replace`, and `--batch-size`. A `--endpoint` mode pointing the read commands at the
gRPC server is still the one documented gap; `src/store_access.rs` is the seam reserved for it. The
SQLite catalog carries a `time_series_readable` view that hex-encodes both hashes for hand
inspection. The read-only gRPC server carries the full read surface too: full `TimeSeriesKey`s over
the wire plus `ListKeys`, `GetMetadata`, `BulkRead`, detailed/per-type counts, `ListOwnerIds`,
`GetIntervals`, static/forecast summaries, `CheckStaticConsistency`, and `ResolveForecastKey`. Auth
is `none` (default) or `api_key` via the `x-api-key` header. See `README.md` and
`docs/src/explanation/data-model.md` for the authoritative feature matrix.

## Code Quality Requirements

**All code changes must pass the following checks before being committed:**

```bash
cargo fmt --all -- --check                              # Rust formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features                   # Tests
dprint check                                             # Markdown formatting
cargo deny check --config deny.toml                     # Dependency policy
```

**Key requirements:**

- **Rust code**: Must compile without clippy warnings. Use
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` to verify.
- **Toolchain**: The workspace uses Rust edition 2024 and declares an MSRV of Rust 1.94 (see
  `rust-version` in the root `Cargo.toml`, which is the sole authority — there is no
  `rust-toolchain` file). Do not use APIs requiring a newer compiler without intentionally updating
  `rust-version` and CI.
- **Pre-commit**: `cargo-husky` installs `.cargo-husky/hooks/pre-commit`, which runs rustfmt,
  Clippy, dprint, and shellcheck when available. Do not bypass a failing hook. Tests and
  `cargo-deny` are still required before committing.
- **CI**: Workspace builds and tests run on Linux, macOS, and Windows. Avoid Unix-only assumptions
  in shared Rust code, build scripts, paths, and workflow changes.
- **Dependency policy**: `deny.toml` rejects wildcard dependencies and unknown registries or Git
  sources. Internal path dependencies must include a version. New licenses must be reviewed before
  adding them to the allowlist.

For detailed style guidelines, see `docs/style-guide.md`.

## Repository Structure

```
crates/
  infrastore-core/    # Types, HDF5 + SQLite storage, hashing, public Rust API
    src/types/               #   array.rs (TypedArray/Dtype), key.rs, metadata.rs, period.rs,
                             #   time_series.rs
    src/storage/             #   memory.rs, hdf5.rs (storage backends)
    src/metadata/            #   schema.rs (SQLite catalog schema)
    src/store.rs             #   Store: the top-level public API
    src/reader.rs            #   StaticReader / ForecastReader: columnar bulk-read surface
    src/hash.rs              #   SHA-256 column hashing
  infrastore-proto/   # Protobuf service definition (proto/) + tonic codegen, conversions
  infrastore-server/  # gRPC server binary (src/bin/server.rs) + Rust client
  infrastore-py/      # PyO3 bindings
  infrastore-ffi/     # C ABI cdylib (used by the Julia binding)
  infrastore-cli/     # `infrastore` CLI: CSV add/read against an on-disk store (clap, csv, tabled)
    src/chart/               #   hand-written sparkline + SVG renderer (no charting dependency)
    src/commands/            #   one module per command group
  infrastore-bench/   # `infrastore-bench` binary: bulk-ingest + simulation-read benchmarks
julia/InfraStore.jl/    # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config and cli/ (sample CSV + descriptor)
.github/workflows/           # Cross-platform tests, linting, security, wheel builds
```

## Build & Test

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Prerequisites

HDF5 and zlib are built from vendored sources and linked statically by default, via the `vendored`
feature that every crate enables (defined in `infrastore-core`, forwarded by the rest). The build
therefore needs `cmake` and a C compiler rather than a system HDF5, plus `protobuf` for the gRPC
codegen:

```bash
brew install cmake protobuf maturin                 # macOS
sudo apt-get install cmake protobuf-compiler        # Linux (Debian/Ubuntu)
```

The first build compiles HDF5 from source (a few minutes), then caches the result.

`--no-default-features` switches back to system libraries, which then need `brew install hdf5` /
`sudo apt-get install libhdf5-dev`, and possibly `HDF5_DIR` if the `hdf5-metno-sys` build script
cannot locate HDF5. Because `hdf5-metno-sys` declares `links = "hdf5"`, Cargo's feature unification
makes vendored-vs-system all-or-nothing across the whole dependency graph — an individual crate
cannot choose independently.

Note that `--all-features` implies `vendored`. The workspace dependencies on `infrastore-core` and
`infrastore-proto` set `default-features = false` in the root manifest, because a workspace member
cannot override an inherited dependency's `default-features`; each member re-enables vendoring
through its own `vendored` feature.

CI provisions no native libraries on any platform, Windows included — the vendored build covers all
three. Do not add a step that exports `HDF5_DIR` in CI: it redirects the vendored build at an
external HDF5 while static libraries are still requested, which fails. Keep these requirements in
mind when changing native dependencies.

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux and Windows those
flags are inert.

## Bindings

### Python (PyO3)

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy tzdata  # tzdata: zoneinfo on Windows
maturin develop --manifest-path crates/infrastore-py/Cargo.toml
pytest python/tests
```

### Julia (C ABI)

```bash
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
# The ZonedDateTime tests need the TimeZones weak dependency, which is only
# loadable through the test target; the run above skips them with a warning:
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.test()'
```

`julia/InfraStore.jl` reads a bare `Dates.DateTime` as a **wall clock** (Julia's carries no zone),
recording `ZonelessReference()` — the stored instant is its own fields, unchanged from the old
UTC-by-convention reading, but the store now records that it _was_ a convention. It also accepts a
`TimeZones.ZonedDateTime` anywhere a timestamp goes, converting it to the instant it names and
recording the spelling its zone names. TimeZones is a **weak dependency**: the conversion methods
live in `ext/InfraStoreTimeZonesExt.jl` and load with `using TimeZones`. Reads still return a
`DateTime` holding the instant, with the reference beside it — changing that would break `IS3.jl`,
which destructures them; `zoned_timestamp` in the extension fuses the two back together.

The FFI build script generates `crates/infrastore-ffi/include/infrastore.h` via `cbindgen`. Never
hand-edit the header. Any change to an exported `extern "C"` function must:

- include an accurate Rustdoc `# Safety` section covering pointer validity, ownership, lengths,
  concurrency, and the matching deallocator;
- regenerate and commit the header;
- update the Julia wrapper and tests when the ABI behavior changes.

## Server

```bash
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .h5, set [authentication]
cargo run -p infrastore-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`; clients must send the chosen key in the
`x-api-key` header.

## Storage Format

- A persisted store is an HDF5 file plus a SQLite catalog at `<store-path>.sqlite`. They are one
  logical artifact and must be moved, copied, and deleted together. The file is written directly
  against libhdf5 (via `hdf5-metno`), not through netcdf-c; the extension is conventionally `.h5`
  but nothing enforces it. Identity comes from the root attribute `storage_backend = "hdf5"`, and
  `Store::open` rejects a file that lacks it — including stores written by the removed netcdf
  backend.
- `CatalogMode` decides where the catalog lives while a store is open, independently of the backend.
  `Attached` (default) makes it the `.sqlite` file, with WAL and durability on every commit.
  `InMemory` holds it in RAM and writes it only at `persist_to` or `persist_catalog`; arrays still
  stream to the HDF5 file, so it does not require the data to fit in memory. It exists for a
  consumer building a store in a scratch directory beside its own volatile state (infrasys does
  exactly this), where a crash loses that state anyway. `MemoryBackend` + `Attached` is rejected.
  `persist_catalog` writes only the `.sqlite` half, stamped to match the HDF5 file already beside it
  — the cheap way to land an in-memory catalog when the arrays are already in place (`persist_to` to
  another path has to copy them). The CLI calls it at the end of every `add`/`init`, because one
  command per process means a catalog still in RAM at exit is lost, not deferred.
- The two halves carry a matching **generation stamp** — the HDF5 root attribute
  `catalog_generation` and the catalog's `catalog_identity` table. `persist_to` stages both halves,
  fsyncs, and renames them into place; because two renames cannot be atomic together, a fresh stamp
  per save makes an interrupted save fail loudly on the next open (`MismatchedArtifact`) instead of
  reading as a valid store. A failed save may still have destroyed the destination — retry from the
  live store rather than assuming the target survived. `compact` rewrites only the HDF5 half and
  must therefore _preserve_ the existing stamp, never mint one. _Both_ halves unstamped (an artifact
  predating the stamp) still opens; exactly _one_ stamped half is a `MismatchedArtifact`, because
  every path that writes a stamp writes both together. Each save stages through a uniquely tagged
  sibling (`<target>.persist-<tag>`, `<store>.h5.repack-<tag>`) — nothing locks a `persist_to`
  destination, so a fixed name would let two savers publish each other's partial files. The cost is
  that an interrupted save's temps are no longer swept by the next one.
- **Creating a store where one already exists is refused** (`TimeSeriesError::StoreExists`),
  checking both halves. Creating truncates the HDF5 file but only _opens_ the catalog, then stamps
  both to match, so without the guard a re-run of a build script produced an empty array file paired
  with the old catalog's rows — opens cleanly, lists every series, every array dangling.
  `create_replacing` (`overwrite=True` / `overwrite=true` in the bindings) is the explicit
  destructive form. `Store::open_copy` copies both halves and opens the copy, so a consumer that
  means to change a user's artifact never attaches to it read-write; HDF5 has no journal, so an
  interrupted in-place write is unrecoverable. Both shipped consumers already do this by hand.
- **Timestamps are millisecond-precision.** A `Period` has always been a whole number of
  milliseconds; every _instant_ the store records (a `SingleTimeSeries` or forecast
  `initial_timestamp`, every entry of a `NonSequentialTimeSeries` vector) is held to the same floor,
  enforced on the write path in `Store`'s `validate_data` and refused with `InvalidParameter` rather
  than truncated. The reason is cross-binding: the C ABI and Julia exchange instants as `i64` Unix
  milliseconds and Python's `datetime` is microsecond, so a finer instant is silently truncated at
  some boundaries and not others. Reads stay permissive so a pre-rule artifact still reads back
  exactly, which is why the rule does not bump `DATA_FORMAT_VERSION`. Query bounds (`time_range`, a
  reader's `when`) are deliberately unconstrained.
- `DATA_FORMAT_VERSION` in `crates/infrastore-core/src/version.rs` is the on-disk compatibility
  contract. Any incompatible HDF5 layout, SQLite schema, dtype encoding, or hashing change must bump
  it and update format documentation and compatibility tests.
- Packed arrays use datasets named `sts_{dtype}_{shape}_{length}_{resolution}` for regular series
  and `nsts_{dtype}_{shape}_{length}_{timestamps_hash}` for the irregular ones sharing a time axis,
  each with a companion `<dataset>_h` hash dataset. Standalone arrays use `arr_{hex_hash}`. A
  `NonSequentialTimeSeries`'s timestamps live in the HDF5 file too, as one `tsv_{hex_hash}` `i64`
  dataset of unix milliseconds per distinct time axis under `time_series/timestamps/`, keyed by the
  same content hash that pools its array. See `crates/infrastore-core/src/storage/hdf5.rs` for the
  implementation and `docs/src/reference/file-format.md` for the user-facing specification; keep
  them synchronized.
- Deletion frees a packed column (slot reusable, hash row and column data zero-filled) or unlinks a
  standalone dataset. HDF5 cannot return the space in place, so the file only shrinks when
  `Store::compact` rewrites it: an on-disk compaction materializes the catalog's live arrays into a
  sibling `<store>.h5.repack` and renames it over the original, assuming a single writer. Compaction
  behavior must remain explicit.

## Conventions

- Keep the multi-language surface consistent: a change to the core public API usually needs matching
  updates across the proto definitions, the gRPC server/client, the PyO3 bindings, and the FFI/Julia
  binding. When adding a feature, check all bindings before considering it done.
- Treat `infrastore-core` as the source of truth. Binding crates depend on core; core must not
  depend on bindings.
- Use `TimeSeriesError` and the shared `Result` alias for core errors. Unsupported operations must
  return an explicit error rather than silently changing semantics.
- Preserve typed-array dtype, shape, byte order, timestamps, features, and hashes across every
  binding and persistence round trip.
- Do not manually edit generated artifacts. Besides the C header, protobuf output is generated by
  the proto crate's build script.
- Keep changes scoped. Do not commit local virtual environments, Python caches, generated HDF5 test
  data, or machine-specific library paths.
