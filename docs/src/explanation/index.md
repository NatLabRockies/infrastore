# Explanation

This section explains how castore is put together and why. It is understanding-oriented: read it to
build a mental model, not to accomplish a specific task. For step-by-step recipes see the
[How-To Guides](../how-to/index.md); for exhaustive listings see the
[Reference](../reference/index.md).

- [Architecture](./architecture.md) — The crates, the two-file storage split, and how the language
  bindings sit on top of a single core.
- [Data Model](./data-model.md) — Owners, time-series types, keys, feature maps, and the
  associations between catalog entities.
- [Storage Model](./storage-model.md) — Why arrays go to NetCDF4 and metadata goes to SQLite, and
  how the two stay consistent.
- [Content Addressing](./content-addressing.md) — How arrays are hashed, deduplicated, and verified.
- [Language Bindings](./bindings.md) — How the Python, Julia, and gRPC interfaces wrap the Rust
  core.
