//! The single seam between the CLI and the store backend.
//!
//! Today this opens the on-disk HDF5 + SQLite artifact directly. A future
//! `--endpoint` flag for the read-only gRPC server would be wired in here
//! without touching the command handlers.

use std::path::{Path, PathBuf};

use infrastore_core::{CatalogMode, Compression, KeyIdentity, Store, create_store, open_store};

/// Where the SQLite catalog lives *while a command runs*.
///
/// Exposed as a flag because the two modes trade crash-durability for speed in
/// a way only the caller can choose. [`CatalogChoice::Attached`] commits (and
/// fsyncs) to `<store>.sqlite` as it goes, so an interrupted load leaves what it
/// had already written. [`CatalogChoice::InMemory`] keeps the catalog in RAM,
/// skipping per-commit journaling — much faster for a bulk load, and losing
/// *everything* if the process dies first, since the arrays streamed to the HDF5
/// file are unreachable without a catalog to name them.
///
/// Either way the command writes the catalog out before it exits
/// (`Store::persist_catalog`), so both modes leave a complete artifact. They
/// cannot differ on that: the CLI runs one command per process, so a catalog
/// still in RAM at exit is not deferred, it is lost — there is no later command
/// that could still write *this* process's catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum CatalogChoice {
    /// The `<store>.sqlite` file, committed on every write.
    #[default]
    Attached,
    /// RAM until `infrastore persist` writes the pair out.
    InMemory,
}

impl CatalogChoice {
    fn mode(self) -> CatalogMode {
        match self {
            CatalogChoice::Attached => CatalogMode::Attached,
            CatalogChoice::InMemory => CatalogMode::InMemory,
        }
    }
}

impl std::fmt::Display for CatalogChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CatalogChoice::Attached => "attached",
            CatalogChoice::InMemory => "in-memory",
        })
    }
}

/// The SQLite catalog paired with an HDF5 store path. Delegates to the core so
/// the CLI cannot drift from the store's own derivation.
pub fn catalog_path(data_path: &Path) -> PathBuf {
    infrastore_core::catalog_sqlite_path(data_path)
}

/// Open an existing store read-only. Errors if the file is missing.
pub fn open_readonly(path: &Path) -> Result<Store, String> {
    if !path.exists() {
        return Err(format!("store not found: {}", path.display()));
    }
    open_store(path, true).map_err(|e| e.to_string())
}

/// Open a writable store, creating it (and its SQLite catalog) if absent.
pub fn open_writable(path: &Path) -> Result<Store, String> {
    open_writable_with(path, None, CatalogChoice::Attached)
}

/// Open a writable store; `compression` applies only when the store is being
/// created here. Passing a compression policy for an existing store is an
/// error — the persisted policy governs and a silent ignore would mislead.
pub fn open_writable_with(
    path: &Path,
    compression: Option<Compression>,
    catalog: CatalogChoice,
) -> Result<Store, String> {
    if path.exists() {
        if compression.is_some() {
            return Err(format!(
                "store {} already exists; its persisted compression policy applies \
                 (drop --compression/--shuffle)",
                path.display()
            ));
        }
        Store::open_with_catalog(path, false, catalog.mode()).map_err(|e| e.to_string())
    } else {
        match (compression, catalog) {
            (None, CatalogChoice::Attached) => create_store(Some(path), false),
            (compression, catalog) => Store::create_with_catalog(
                Some(path),
                false,
                compression.unwrap_or_default(),
                catalog.mode(),
            ),
        }
        .map_err(|e| e.to_string())
    }
}

/// Remove the identities in `keys` that the store actually holds, ignoring the
/// rest. Returns the number of associations removed.
///
/// This is what `--replace` means on a load: "if a series with this identity is
/// already here, drop it first". [`Store::remove_time_series_bulk`] deliberately
/// cannot express that — it is all-or-nothing and fails the whole batch with
/// `NotFound` if *any* key is absent, which is the right contract for a caller
/// that is removing a set it believes exists. Passing a load's identities to it
/// makes `--replace` fail on the first load into a new store, and drop the whole
/// batch whenever a descriptor introduces one new series.
///
/// One `has_time_series` probe per identity, on the same connection the removal
/// then uses. The CLI is a single writer in a single process, so nothing can
/// insert between the probe and the remove.
pub fn remove_existing(store: &mut Store, keys: &[&KeyIdentity]) -> Result<usize, String> {
    let mut present: Vec<&KeyIdentity> = Vec::new();
    for key in keys {
        if store.has_time_series(key).map_err(|e| e.to_string())? {
            present.push(key);
        }
    }
    if present.is_empty() {
        return Ok(0);
    }
    store
        .remove_time_series_bulk(&present)
        .map_err(|e| e.to_string())
}
