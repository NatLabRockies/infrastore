//! The single seam between the CLI and the store backend.
//!
//! Today this opens the on-disk NetCDF + SQLite artifact directly. A future
//! `--endpoint` flag for the read-only gRPC server would be wired in here
//! without touching the command handlers.

use std::path::Path;

use infrastore_core::{Compression, Store, create_store, open_store};

/// Open an existing store read-only. Errors if the file is missing.
pub fn open_readonly(path: &Path) -> Result<Store, String> {
    if !path.exists() {
        return Err(format!("store not found: {}", path.display()));
    }
    open_store(path, true).map_err(|e| e.to_string())
}

/// Open a writable store, creating it (and its SQLite catalog) if absent.
pub fn open_writable(path: &Path) -> Result<Store, String> {
    open_writable_with(path, None)
}

/// Open a writable store; `compression` applies only when the store is being
/// created here. Passing a compression policy for an existing store is an
/// error — the persisted policy governs and a silent ignore would mislead.
pub fn open_writable_with(path: &Path, compression: Option<Compression>) -> Result<Store, String> {
    if path.exists() {
        if compression.is_some() {
            return Err(format!(
                "store {} already exists; its persisted compression policy applies \
                 (drop --compression/--shuffle)",
                path.display()
            ));
        }
        open_store(path, false).map_err(|e| e.to_string())
    } else {
        match compression {
            Some(c) => Store::create_with_compression(Some(path), false, c),
            None => create_store(Some(path), false),
        }
        .map_err(|e| e.to_string())
    }
}
