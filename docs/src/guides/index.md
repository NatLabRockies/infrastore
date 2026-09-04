# Developer Guides

These guides are written for developers building on infrastore from a specific language. Each one
starts at installation and walks the full workflow — create or open a store, add series, query,
read, and persist — with the idioms of that language, then points at the matching
[reference](../reference/index.md) for exact signatures.

- [Rust](./rust.md) — Embed `infrastore-core` directly.
- [Python](./python.md) — Use the `infrastore` PyO3 wheel.
- [Julia](./julia.md) — Use the `InfraStore.jl` package over the C ABI.
- [CLI](./cli.md) — Load and inspect a store from a terminal.
- [gRPC Server & Client](./server.md) — Serve a store for remote readers.
- [Benchmarks](./benchmarks.md) — Measure bulk-add and simulation-loop read performance.

If you are building a package on top of infrastore, read
[Embedding in a Parent Package](./embedding.md) first: it collects the contracts and the lifecycle
these guides only show the individual calls for.

For the concepts underneath all of them, see the [Explanation](../explanation/index.md) section —
especially [Time-Series Types](../explanation/time-series-types.md),
[Data Model](../explanation/data-model.md), and [Readers](../explanation/readers.md), which apply
equally to every binding.
