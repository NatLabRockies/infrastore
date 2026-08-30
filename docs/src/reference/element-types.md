# Element Types

Every stored array carries an **`element_type`**: what its elements mean, and — for the composite
kinds — how one timestep's values are laid out across the array's trailing dimensions.

It is a first-class, store-owned concept, not a binding convention. A Julia `PiecewiseLinearData`, a
Python list of `{"x": …, "y": …}` dicts, and a TypeScript `{x, y}[]` are all `piecewise_linear`
here, so a consumer written in any of those languages can decode an array without knowing which
language wrote it.

`element_type` **replaces** a separate physical `dtype` column: the dtype of the stored bytes is
derived from it. `TypedArray` still carries a `dtype`, because it describes bytes; the element type
lives on the association metadata and on the write API, where interpretation belongs.

## Canonical string form

The element type travels as a string: the `element_type` column in the SQLite catalog, a UTF-8
string across the C ABI, and a `string` field over gRPC. A parameterized grammar does not fit an
integer code, so unlike `dtype` there is no numeric encoding.

```text
f64 | f32 | i64 | i32 | i16 | i8 | u64 | u32 | u16 | u8 | bool
tuple(N,dtype)          e.g. tuple(3,f64)
linear_function
quadratic_function
piecewise_linear
piecewise_step
```

The names are deliberately language-neutral: `piecewise_linear`, not `PiecewiseLinearData`;
`tuple(3,f64)`, not `NTuple{3, Float64}`. Each binding maps them to its own domain types.

## Encoding

The first array dimension is time (the per-window horizon for forecasts). The trailing dimensions
are the per-step element shape, determined by the element type:

| `element_type`       | element shape   | row layout (one timestep)                       |
| -------------------- | --------------- | ----------------------------------------------- |
| a dtype spelling     | `[]`            | the value                                       |
| `tuple(N,T)`         | `[N]`           | `t1 … tN`                                       |
| `linear_function`    | `[2]`           | `proportional, constant`                        |
| `quadratic_function` | `[3]`           | `quadratic, proportional, constant`             |
| `piecewise_linear`   | `[1 + 2*w]`     | `n, x1, y1, …, xn, yn`, zero-padded to width    |
| `piecewise_step`     | `[max(1, 2*w)]` | `n, x1 … xn, y1 … y(n-1)`, zero-padded to width |

`w` is the maximum point / x-coordinate count across the series (or across every window of a
forecast). Ragged rows are self-describing through the leading count `n`, so decoding one row needs
no global state.

A scalar element type still allows a dense per-step array — a `Probabilistic` forecast's percentile
columns, say. The dense-array case and the `tuple(N,T)` case differ in meaning, not bytes: the tuple
says the `N` values are one composite value, not `N` independent samples.

Forecasts stack windows in front of the per-step element shape: `[H, count, *E]` for a
`Deterministic`, `[P, H, count, *E]` for a `Probabilistic` or `Scenarios`. The per-row scheme is
unchanged; only how many leading axes precede it differs (`TimeSeriesType::leading_dims`).

Every function-data kind is stored as `f64`.

## Validation

Because the store owns the element type, it can reject a write that contradicts it rather than
storing the inconsistency blindly:

- the array's dtype must equal the element type's physical dtype;
- fixed-width kinds must have exactly their per-step dims (`[2]`, `[3]`, `[N]`);
- ragged kinds must have exactly one trailing dim, of a width their layout can produce (odd for
  `piecewise_linear`; 1 or even for `piecewise_step`);
- every ragged row's leading count `n` must be a non-negative whole number that fits the row.

Scalars are unconstrained in their trailing dims, since a dense per-step array is legitimate.

Every series carries a concrete element type — a constructor resolves it to plain scalars of the
array's dtype, and declaring one replaces it — so the checks above run on every write, not only on
writes that declared something. There is no "undeclared" state to fall back from.

## The catalog is the source of truth

Nothing about element typing is recoverable from the HDF5 file. Storage records how many bytes an
element occupies; `element_type` records what it means, and even the physical dtype is not fully
recoverable (`bool` and `u8` are the same byte). Every read therefore resolves the element type from
the catalog first and tells the storage backend what to decode — the backend never infers it.

`infrastore verify` follows from this: it walks the arrays the _catalog_ references, so a catalog
row pointing at an array the file does not hold, or a row too malformed to name one, is reported. An
array in the file that no association references is not checked — it is unreachable, and nothing
records what its bytes mean.

## Codecs

Each binding ships a reference codec between the stored bytes and per-timestep values:

- **Rust** — `infrastore_core::{decode, encode}` over `TypedArray` + `ElementType`. Prefer the
  paired forms: every value type has a `from_values` constructor that encodes the values _and_
  declares the element type they imply, and `TimeSeriesData::decoded_values` reads them back. An
  `element_type` and the array it describes are two things a caller can get out of step — the store
  rejects the mismatch on write, but deriving both from one set of values means there is none to
  reject.
- **Python** — `infrastore.decode_element_values(array, element_type, leading_dims)`. Decode only; a
  write still encodes by hand and declares `element_type=`.
- **TypeScript** — `@infrastore/codec`, which decodes a gRPC response's `value_bytes` + `shape` +
  `element_type` directly into plottable values.
- **Julia** — no codec yet. A read hands back the raw array with its `element_type` beside it, and
  InfrastructureSystems.jl decodes to its own `FunctionData` types.

The CLI is deliberately not on that list: it renders function-data rows as their raw padded numbers
rather than decoding them.

`conformance/element_type_vectors.json` at the repo root pins encoded bytes against expected decoded
values for every element type, static and forecast. It is generated from `infrastore-core`'s
`codec::conformance` vectors and read by every binding's codec tests, so all four implementations
are held to one definition of the encodings rather than to each other. Regenerate it with:

```bash
UPDATE_CONFORMANCE_VECTORS=1 cargo test -p infrastore-core conformance
```

A binding may reject what it cannot represent — the grammar allows `tuple(4,i32)`, which the Julia
binding does not map — but the store accepts the full grammar.

Because consumers go through the codecs, a future storage optimization (a true ragged layout with an
offsets array instead of zero padding) can land behind this boundary without touching them.
