# Explanation

This section explains how infrastore is put together and why. It is understanding-oriented: read it
to build a mental model, not to accomplish a specific task. For the calls that do the work see the
[Developer Guides](../guides/index.md); for exhaustive listings see the
[Reference](../reference/index.md).

- [Architecture](./architecture.md) — The crates, the two-file storage split, and how the language
  bindings sit on top of a single core.
- [Design Choices](./design-choices.md) — What infrastore optimizes for and why, written for
  developers of parent packages like IS.jl and infrasys.
- [Time-Series Types](./time-series-types.md) — The six types, which one your data wants, and the
  vocabulary they share: periods, timestamp precision, typed arrays.
- [Data Model](./data-model.md) — Owners, features, identity, association ids, and the associations
  between catalog entities.
- [Time References](./time-references.md) — How a series' timestamps are spelled, what that does and
  does not change, and why a named zone is safe.
- [Readers](./readers.md) — The columnar bulk-read surface: why it exists, what one timeline per
  reader means, and when to reach for something else.
- [Storage Model](./storage-model.md) — Why arrays go to HDF5 and metadata goes to SQLite, and how
  the two stay consistent.
- [Content Addressing](./content-addressing.md) — How arrays are hashed, deduplicated, and verified.
- [Language Bindings](./bindings.md) — How the Python, Julia, and gRPC interfaces wrap the Rust
  core.
