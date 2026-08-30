# Function data as first-class values

Scoping notes for making the function-data element types (`linear_function`, `quadratic_function`,
`piecewise_linear`, `piecewise_step`) and `tuple(N,dtype)` first-class **values** in the Rust core
and in InfraStore.jl, so that a series of curves round-trips as curves rather than as a padded
`Float64` matrix the caller decodes by hand.

Written 2026-08-30, after parameterizing `TimeSeriesMetadata.time_series_type`.

**The motivating gap.** After the `{T,N}` change, a metadata row names exactly what a read hands
back — except for the function-data kinds, where both say `Float64`:

```julia
md.time_series_type   # SingleTimeSeries{Float64, 2}, element_type == "piecewise_linear"
```

Nothing is lost on disk (`element_type` + `element_shape` fully describe the encoding, and the Julia
read path already carries `element_type` back on the value struct, `operations.jl:556`), but the
consumer — InfrastructureSystems.jl, which has its own `PiecewiseLinearData` — has to decode the
padded rows itself. The goal is that it does not.

## Where we are today

Verified against the tree at `6f0e04f`.

| Layer           | Element-type support                                                                                                               |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **Rust core**   | Complete. `ElementType` (6 variants, parse/display/validate) in `types/element_type.rs`; a full codec in `codec.rs`                |
| **Conformance** | `conformance/element_type_vectors.json` — 10 vectors, generated from `codec/conformance.rs`, covering every kind, forecasts, empty |
| **TypeScript**  | Full decode, own implementation, conformance-tested (`typescript/codec/src/index.ts`, 340 lines)                                   |
| **Python**      | Decode only — `decode_element_values(data, element_type, leading_dims)` (`infrastore-py/src/lib.rs:3980`). No encode               |
| **Julia**       | **Nothing.** `_physical_dtype_of` recognizes the spellings to derive a dtype (`types.jl:51`); values stay raw                      |
| **C ABI**       | Nothing beyond passing the `element_type` string through                                                                           |
| **CLI**         | Nothing — it renders via `csv_io::array_to_f64_lossy`, not the codec                                                               |

So a large part of "formalize in Rust" already exists and is battle-tested. `codec.rs` defines
`XyPoint`, `LinearFunction`, `QuadraticFunction`, `StepFunction`, and the `DecodedValues` enum, with
`encode` / `decode` / `element_type_of`, all re-exported from `lib.rs:28`.

Two gaps on the Rust side:

1. **The write path never mentions them.** A producer builds the flat array itself, or calls
   `encode(&values, &[len])` and then remembers `\.with_element_type(ElementType::PiecewiseLinear)`
   — two independent steps that can disagree. (`validate_data` catches the disagreement at `add`
   time, so nothing corrupt is stored; it is an ergonomics gap, not a safety one.)
2. **The read path never mentions them.** `read_by_id` returns a `TypedArray`; the caller pairs it
   with the row's `element_type` and calls `decode` with the right `leading_dims`.

One documentation inaccuracy to fix in passing: `docs/src/reference/element-types.md` says the Rust
codec is "used by the CLI for human-readable dumps". It is not — the CLI has its own lossy path.

## Target state

```julia
curves = [PiecewiseLinearData([(1.0, 10.0), (2.0, 20.0)]),
          PiecewiseLinearData([(1.0, 11.0), (2.5, 21.0), (4.0, 30.0)])]

id = add_time_series!(store, 1, "Generator", Component,
                      SingleTimeSeries(t0, Hour(1), curves, "variable_cost"))

ts = read_by_id(store, id)                        # SingleTimeSeries{PiecewiseLinearData, 1}
ts.data[2].points[3]                              # (x = 4.0, y = 30.0)
get_metadata_by_id(store, id).time_series_type    # SingleTimeSeries{PiecewiseLinearData, 1}
```

with `element_type == "piecewise_linear"` on the row exactly as today, the same bytes on disk, and
the same `data_hash` — so this is a **binding-and-API change, not a format change**.
`DATA_FORMAT_VERSION` does not move.

## Decisions to make first

These five gate the work. My recommendation is in each heading; the reasoning follows.

### D1 — Julia decodes in Julia, not through the FFI ✅ recommend binding-side

|                 | Binding-side (pure Julia)                         | Through the C ABI                             |
| --------------- | ------------------------------------------------- | --------------------------------------------- |
| ABI change      | none                                              | 2+ new exports, header regen, `# Safety` docs |
| Cost per read   | one pass over the array already copied into Julia | serialize → FFI → JSON → parse, per read      |
| Implementations | a 5th copy of the row layouts                     | one                                           |
| Precedent       | TypeScript already does exactly this              | —                                             |

The conformance corpus exists precisely to make the first option safe, and
`docs/src/reference/element-types.md` already anticipates it ("Each binding ships a reference
codec"). The alternative puts a JSON round trip on the read path of the hot consumer. Take the
binding-side codec, and hold it to `conformance/element_type_vectors.json` like the other three.

### D2 — the core gains typed constructors ✅ recommend yes, thin

"Formalized in Rust" should mean a Rust caller never pairs `encode` with `with_element_type` by
hand. Add to each of the five value types a constructor that takes values and derives both:

```rust
SingleTimeSeries::from_values(t0, resolution, &DecodedValues::PiecewiseLinear(curves))?
Deterministic::from_values(t0, resolution, horizon, interval, count, &values)?
```

Each is `encode(values, leading_dims)` + `element_type_of(values)` + the existing `new`, so it is
mechanical — but it makes the invariant unrepresentable-if-broken rather than merely validated. Pair
with `TimeSeriesData::decoded_values(&self) -> Result<DecodedValues>` on the read side.

Deliberately **not** in scope: making `TypedArray` itself generic over logical elements, or adding a
`DecodedValues` variant to `TimeSeriesData`. The flat array is the storage truth and the reader's
hot path; keeping the logical view a projection of it is what lets `StaticReader` stay raw.

### D3 — move the codec down, leave the domain types where they are ✅ **resolved**

Resolved against the IS.jl 3.6.0 checkout at `~/repos/sienna/InfrastructureSystems.jl`. The original
question — "should `PiecewiseLinearData` and friends move into InfraStore.jl and infrastore?" —
turns out to have two halves with opposite answers.

**The encoding should move. It is already written, one layer too high.** `IS/src/infrastore.jl`
(2513 lines) carries a complete Julia codec: `_storage_width` / `_storage_array` for encode,
`_element_encoding` / `_decode_static_values` / `_decode_forecast_window` for decode, over all four
function-data kinds plus `NTuple{N,Float64}`, statics and forecasts. Its layouts are
**byte-identical** to the Rust codec — `1 + 2·max_points` for `piecewise_linear`, `max(2n, 1)` for
`piecewise_step`, same field order, same zero padding — and it is already tested against
`conformance/element_type_vectors.json`, vendored at `IS/test/conformance/` with a live-path
override for infrastore devs. So a Julia codec is not a thing to write; it is a thing to lift.

**The domain types should not move.** Three findings, each sufficient on its own:

1. **`FunctionData` spans a reference type.** `TimeSeriesFunctionData{T} <: FunctionData` holds a
   `ConcreteTimeSeriesKey`. Moving the hierarchy would drag IS.jl's keyed-reference model into
   InfraStore.jl — exactly what infrastore removed as an address. Only the four `StaticFunctionData`
   leaves are even candidates.
2. **The validation diverges on purpose.** IS.jl's `PiecewiseLinearData` requires **at least two
   ascending x-coordinates**; the store accepts zero- and one-point rows, and has conformance
   vectors for both. `conformance.rs` already names the divergence in a comment, and IS.jl's test
   already skips those vectors through an `_is_representable` predicate. They are not the same type:
   one is a storage encoding, the other a validated domain object.
3. **~1000 lines of domain algorithms ride on them** — `convexity_checks.jl` (369) and
   `make_convex.jl` (611). Convexity is an optimization concern with no business in a store.

**Correction to the earlier recommendation:** InfraStore.jl must _not_ use the IS.jl spellings. Both
packages exporting `PiecewiseLinearData` makes `using InfraStore, InfrastructureSystems` an
ambiguity error. InfraStore.jl's own types take the wire vocabulary's names — `LinearFunction`,
`QuadraticFunction`, `PiecewiseLinear`, `PiecewiseStep`, matching the Rust core.

**So that a consumer still pays no conversion**, the lifted codec is generic over its target
constructors rather than hardcoding InfraStore.jl's types: InfraStore.jl's permissive types are the
default, and IS.jl plugs in its own through a package extension, the same shape as
`InfraStoreTimeZonesExt`. IS.jl then decodes straight to `IS.PiecewiseLinearData` with no
intermediate allocation, and deletes its own copy of the layouts. This is a small generalization of
structure IS.jl already has — `_ELEMENT_ENCODINGS` is exactly that dispatch table.

**A benefit of the direction:** IS.jl's `PiecewiseStepData` constructor still carries an HDF-era
serialization hack — it prepends/strips a `NaN` so `x` and `y` lengths match, and there is a
`PiecewiseStepData(::AbstractMatrix)` "for HDF deserialization". The store's `n, x…, y…` layout is
self-describing and needs neither. Moving the encoding down lets a domain type shed storage
compromises.

### D4 — reads decode by default, with a raw escape hatch ⚠️ breaking

Today a piecewise row reads back as `SingleTimeSeries{Float64,2}`. After this it is
`SingleTimeSeries{PiecewiseLinearData,1}`. Any consumer decoding by hand today breaks.

Recommend the hard switch plus `read_by_id(store, id; raw = true)` for anyone who wants the padded
matrix (and for round-tripping a series whose element type the binding does not map). The
alternative — decode only on request — leaves the default output still mistyped and defeats the
goal.

This lands in the same release as the `md.time_series_type == X` → `<: X` migration, so IS3.jl takes
one break rather than two. **Both should be called out together in the changelog.**

**IS.jl must migrate in lockstep.** It calls `InfraStore.read_by_id` and then decodes the result
itself (`_decode_stored_values(sts.data, _element_encoding(sts.element_type))`,
`IS/src/infrastore.jl:941`). A read that decodes by default would hand it values it would try to
decode a second time. Either IS.jl moves to the lifted codec in the same release, or it passes
`raw = true` until it does — the escape hatch is what makes a staged migration possible at all.

### D5 — `tuple(N,dtype)` ✅ recommend map only `tuple(N,f64)` → `NTuple{N,Float64}`, or defer

`element-types.md` currently states the Julia binding does not map tuples, and the grammar allows
`tuple(4,i32)`. Mapping the `f64` case is nearly free once the codec exists (`DecodedValues::Tuple`
is already `Vec<Vec<f64>>`); mapping every dtype means a codec generic over the physical width.
Decide, then make the doc match — it is currently a promise about behavior.

## The rank rule, restated

The `{T,N}` rule shipped in `_parameterized_type` (`catalog.jl`) is:

```
N = length(element_shape) + 1          (+2 for DeterministicSingleTimeSeries)
```

A function-data kind consumes **exactly one** trailing dim, which the logical type absorbs:

| Row                                    | `element_shape` | today                         | after                                     |
| -------------------------------------- | --------------- | ----------------------------- | ----------------------------------------- |
| `SingleTimeSeries`, `piecewise_linear` | `(7,)`          | `SingleTimeSeries{Float64,2}` | `SingleTimeSeries{PiecewiseLinearData,1}` |
| `Deterministic`, `quadratic_function`  | `(3,3)`         | `Deterministic{Float64,3}`    | `Deterministic{QuadraticFunctionData,2}`  |
| `Probabilistic`, `piecewise_linear`    | `(2,1,5)`       | `Probabilistic{Float64,4}`    | `Probabilistic{PiecewiseLinearData,3}`    |

So for the mapped kinds `N = length(element_shape)`, and `T` is the domain type rather than the
physical dtype. That is a change to **one function** — `_parameterized_type` — which is why the
`{T,N}` work landing first was worth doing in that shape.

**Axis order (a trap worth writing down).** Julia's read path permutes row-major dims back to
column-major (`_decode_array`, `types.jl`), so a stored `[3,7]` array becomes a Julia `(3,7)` array:
time first, element slots **last**. The element axis is the last axis in Julia too, for statics and
forecasts alike (`Deterministic` is `(H, count, *E)`, `Probabilistic`/`Scenarios`
`(P, H, count,
*E)`). The Julia codec therefore folds the final axis and leaves the leading ones
alone — the same shape of operation as Rust's, not a transposed one.

## Phases

Each phase is independently landable and leaves the tree green.

### Phase 0 — decide (blocking)

D1–D5 above. D3's open question needs an IS.jl version pinned down. Everything else is mechanical
once these are settled.

### Phase 1 — Rust core ergonomics ✅ **done**

- `from_values` on all five value types, and `TimeSeriesData::decoded_values()`
  (`types/time_series.rs`). Each `from_values` routes through one private `encode_with_type`, so the
  array and the element type are derived from a single input and cannot disagree. `DecodedValues`
  gained `len` / `is_empty` so a static constructor infers `length` from the values.
- The conformance corpus went from 10 vectors to **15**. The four planned
  (`linear_function_deterministic`, `tuple3_deterministic`, `quadratic_function_scenarios`,
  `piecewise_linear_static_single_point`) plus `piecewise_step_single_coordinate` — one x-coordinate
  and therefore _no_ y-values, the `n - 1` rule at its boundary, which a decoder assuming at least
  one y gets wrong. Every `leading_dims` of 1/2/3 is now covered for both a fixed-width and a ragged
  kind, and `Scenarios` appears at all.
- `element-types.md`: the CLI claim is corrected (it renders raw padded numbers), the Rust entry now
  points at the paired forms, and the Julia entry says plainly that there is no codec yet.

_Verified:_ `cargo fmt`, `clippy -D warnings`, the full workspace suite (43 binaries), `cargo deny`,
and both doctests. The **existing Python and TypeScript decoders passed the five new vectors
unchanged** — a useful signal that the new edges are consistent with the encodings already shipped,
not new behavior.

_Touched:_ `infrastore-core` and the corpus only. No ABI, proto, or header regeneration; no binding
changes.

### Phase 2 — lift IS.jl's codec into InfraStore.jl ✅ **done**

- `julia/InfraStore.jl/src/function_data.jl`: `LinearFunction`, `QuadraticFunction`,
  `PiecewiseLinear`, `PiecewiseStep`, plus `XYCoords` — a `@NamedTuple{x, y}` rather than a struct,
  so a consumer's own point type is already the same value. Permissive, matching what the store
  accepts; wire-vocabulary names, so they cannot clash with IS.jl's exports.
- `encode_element_values` / `decode_element_values`, pure functions with no `Store` in the
  signature. Decode returns the array's shape **without** its trailing element axis, so a forecast
  comes back windowed rather than flattened — the same rank rule `_parameterized_type` follows.
- Tests read `conformance/element_type_vectors.json` directly and check **all 15 vectors in both
  directions**, including the three forecast layouts IS.jl's own conformance test skips
  (`leading_dims == 1` only). The Phase 1 vectors finally have a Julia consumer.

**Correction to the plan: the extension cannot live on the InfraStore side.** A package extension
needs a weak dependency on InfrastructureSystems.jl, and IS.jl already depends on InfraStore — that
is a cycle Pkg will not resolve. It is also unnecessary. Two extension points do the same job
without one:

- **Decode** takes a `types` keyword (`DEFAULT_ELEMENT_TYPES` by default), because it starts from an
  `element_type` string and the type has to be chosen by name. The entries' call signatures are
  exactly IS.jl's `FunctionData` constructors, which is what makes the substitution free.
- **Encode** is open dispatch: `element_type_tag`, `element_row_width` and `write_element_row!`,
  three small generic functions a consumer adds methods to for its own types.

IS.jl adds all of that on its own side, where it already has the dependency.

_Size:_ ~400 lines of source, ~150 of tests. _Verified:_ the full InfraStore.jl suite, `Pkg.test()`,
and the Julia formatter.

### Phase 3 — Julia write path ✅ **done**

- The five constructors accept `Vector{<:FunctionData}` (and the forecast ranks) and name the
  `element_type` from the values, so `element_type=` is now only for the numeric case. A declaration
  that contradicts the values is an error rather than an override — the values are the more specific
  statement.
- The struct keeps the _values_; encoding happens at the ABI boundary in one helper, `_wire_array`,
  which the four batch sites now share. That is what makes the write and read symmetric, and what
  lets a metadata row's `{T,N}` describe the values rather than their packing.

### Phase 4 — Julia read path and metadata typing ✅ **done**

- `read_by_id` / `read_by_ids` decode composite element types into their values, with `raw = true`
  for the packing — one axis more, held as the physical dtype.
- `_parameterized_type` maps a composite spelling to its domain type and drops one from the rank, so
  `md.time_series_type == typeof(read_by_id(store, id))` now holds for the composite kinds too, not
  just the numeric ones. An unmapped spelling keeps describing the stored numbers.
- **The readers stay raw**, as planned: `StaticReader` / `ForecastReader` are the per-timestamp
  simulation path and `StaticGroup.dtype` is physical by definition. Documented in `julia-api.md`
  rather than left to be discovered.

_Verified:_ all five composite kinds round-trip as values, static and forecast, plus the `raw`
escape hatch and the untouched numeric path. One existing assertion changed meaning by design — a
`tuple(3,f64)` row is now `SingleTimeSeries{NTuple{3,Float64}, 1}` rather than
`SingleTimeSeries{Float64, 2}` — and was updated with the `raw` form asserted beside it.

### Phase 5 — Python parity ✅ **done**

`encode_element_values(values, element_type, leading_dims)` alongside the decoder it inverts, taking
the same payload shapes the decoder produces, so a Python round trip needs no reshaping in between.
`leading_dims` is the _shape_ of the leading axes and defaults to the static case. A scalar
`element_type` is refused rather than answered with an array that looks like a packing: the numbers
are already the values.

Tested against the corpus in both directions — all 15 vectors — plus the pair a caller actually
uses: encode, store, read, decode.

Not done, and deliberately: typed value objects for Python. The decoder returns dicts and lists,
which is what a numpy-shaped consumer wants; dataclasses would be a second representation to keep in
step for no gain that showed up here.

### Phase 6 — CLI, docs ✅ **done**

- `get -f json` now carries an `element_values` key beside `values`, holding the self-describing
  decoded form (`{"kind": "piecewise_linear", "timesteps": …}`) for a composite row.
- **CSV stays packed on purpose.** That form is what `add` reads back, so it has to remain the
  store's own layout rather than a rendering of it. Written down in `element-types.md` rather than
  left as an accident of which code path was touched.
- `element-types.md` gains an "Extending the codec" section: the two directions extend differently
  because they start from different things — decoding from a tag string, so a `types` table;
  encoding from a value, so open dispatch — and `is_element_values` answers by asking whether those
  methods exist, so opting a type in is exactly defining them.

### IS.jl migration ✅ **done** (the lift's other half)

`InfrastructureSystems.jl/src/infrastore.jl`: **−374/+139**, the additions being twelve small encode
methods and the decode table. Deleted: `_storage_array`, `_storage_width`,
`_storage_forecast_array`, `_element_type_name`, the whole `ElementEncoding` hierarchy,
`_element_encoding`, `_decode_static_values`, `_decode_stored_values`, `_decode_forecast_window`,
`_decode_ntuples`, `_decode_pwl_step_row`, `_decode_element`, `_forecast_window`.

Three parts needed thought rather than deletion. The `Deterministic` write collapsed to
`reduce(hcat, windows)` — the dict-to-matrix densification is IS's concern, the element packing is
the store's. `_dense_forecast_array` survives for `Probabilistic`/`Scenarios`, minus its tagging.
And the **reader paths still decode**, because the store's readers deliberately hand back packing —
but through `InfraStore.decode_element_values` now, with the reader structs caching a tag string
instead of an encoding singleton.

One API addition made it possible: `read_by_id` / `read_by_ids` take `types`, so IS's single read
site lands directly in `LinearFunctionData` and friends.

**The bug worth remembering:** the write path first decided "are these domain values?" with a fixed
`Union` of _this package's_ types. A consumer's opted-in types are not in it, so IS's values fell
through to the numeric path and threw `unsupported element dtype LinearFunctionData`. The test has
to be dispatch-based — `is_element_values` asks whether the three encode methods exist — or the
extension point only works for types the package already knows, which is not an extension point.

## Risks and sharp edges

1. **Two breaking changes for IS3.jl in one release** — the metadata `<:` migration (already
   shipped) and the read type change (D4). Land them in one version with one migration note.
2. **Not a fifth encoder — a fourth, moved.** There are already two Julia-side encoders' worth of
   risk here: IS.jl's, and the one Phase 2 was going to write. Lifting IS.jl's leaves the same
   number of implementations as today and removes the copy sitting above the storage boundary. The
   corpus still had to grow first (Phase 1), so the lifted code is held to cases Rust did not pick
   for itself.
3. **Ragged counts travel as `f64`.** `n` is stored in the row's leading slot as a float and read
   back with `r[0] as usize`. Exact below 2^53, so it is not a practical limit — but the Julia codec
   must round-trip it the same way rather than storing a separate integer.
4. **Empty rows.** `piecewise_step_all_empty` (shape `[2,1]`) exists in the corpus because an empty
   step function has one slot holding a zero count. A naive Julia decoder that assumes `2n` slots
   breaks on it. Similarly, `decode` refuses a zero-width `tuple` explicitly (`codec.rs`).
5. **`element_type` is still descriptive** — outside `KeyIdentity` and both content hashes. Two
   series differing only in it remain duplicates of each other. Nothing here changes that, and the
   plan must not accidentally make the logical type part of identity.
6. **IS.jl vendors the corpus.** `IS/test/conformance/element_type_vectors.json` is a copy, now at
   10 vectors against the repo's 15. It is vendored on purpose — so IS.jl's parity check runs
   without an infrastore checkout — and its live-path override (`INFRASTORE_CONFORMANCE_DIR`, or a
   sibling checkout) means an infrastore dev already tests against the new corpus. Re-vendor when
   convenient; nothing breaks meanwhile. Verified: IS.jl's codec passes all 15, skipping the two
   single-point vectors through its `_is_representable` predicate and the three forecast ones
   through its `leading_dims == 1` filter.
7. **Unmapped spellings must stay readable.** A row whose `element_type` this binding does not map
   (`tuple(4,i32)` under D5) has to keep reading back as a raw array rather than throwing — the same
   forward-compatibility rule `_parameterized_type` already follows for an unrecognized spelling.

## Effort

| Phase                         | Size    | Blocking?       |
| ----------------------------- | ------- | --------------- |
| 0 — decisions                 | —       | yes, everything |
| 1 — Rust ergonomics + vectors | ✅ done | —               |
| 2 — Lift IS.jl's codec        | ✅ done | —               |
| 3 — Julia write               | ✅ done | —               |
| 4 — Julia read + metadata     | ✅ done | —               |
| 5 — Python parity             | ✅ done | —               |
| 6 — CLI + docs                | ✅ done | —               |

All six phases are done, along with the IS.jl migration that makes Phase 2 a lift rather than a
fifth implementation.
