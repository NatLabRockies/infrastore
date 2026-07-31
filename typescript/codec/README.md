# @infrastore/codec

Decode [infrastore](../../README.md) time-series arrays into per-timestep values.

The read-only gRPC server ships raw bytes — `value_bytes` plus a `shape` and an `element_type`
string — which keeps payloads compact and the server simple, and leaves exactly one decode
implementation per language. This package is that implementation for TypeScript, with no runtime
dependencies.

```ts
import { decodeElementValues } from "@infrastore/codec";

const resp = await client.getTimeSeries({ key });
const decoded = decodeElementValues(resp.valueBytes, resp.shape, resp.elementType);

if (decoded?.kind === "piecewise_linear") {
  // One `{x, y}[]` per timestep — directly plottable.
  for (const points of decoded.timesteps) {
    chart.addSeries(points);
  }
}
```

`decodeElementValues` returns `null` when there is nothing to decode: a scalar element type, or any
array whose physical dtype is not `f64`. There the stored elements already are the values, so read
them out of `value_bytes` with the matching typed-array view.

`leadingDims` (the fourth argument, default `1`) is how many leading axes precede the per-step
element shape: `1` for a static series, `2` for a `Deterministic`, `3` for a `Probabilistic` or
`Scenarios`.

## Element types

See [the element-type reference](../../docs/src/reference/element-types.md) for the encodings and
the canonical string grammar.

## Tests

```bash
npm test
```

The tests read `conformance/element_type_vectors.json` at the repo root — the same corpus the Rust,
Python, and Julia codec tests read, so all four implementations are held to one definition of the
encodings rather than to each other. Node 22.6+ is required (the tests run TypeScript directly via
type stripping; there is no build step).
