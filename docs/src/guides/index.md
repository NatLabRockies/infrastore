# Developer Guides

These guides are written for developers building on time-series-store from a specific language. Each
walks through the full workflow — create or open a store, add series, query, read, and persist —
with the idioms of that language, then points at the matching [reference](../reference/index.md) for
exact signatures.

- [Rust](./rust.md) — Embed `time-series-store-core` directly.
- [Python](./python.md) — Use the `time_series_store` PyO3 wheel.
- [Julia](./julia.md) — Use the `TimeSeriesStore.jl` package over the C ABI.
- [gRPC Server & Client](./server.md) — Serve a store for remote readers.
- [Benchmarks](./benchmarks.md) — Measure bulk-add and simulation-loop read performance.

If you just want a task-sized recipe (install, wire up a binding), see the
[How-To Guides](../how-to/index.md). For the concepts underneath all of these, read the
[Explanation](../explanation/index.md) section — especially
[Data Model](../explanation/data-model.md) and
[Content Addressing](../explanation/content-addressing.md), which apply equally to every binding.
