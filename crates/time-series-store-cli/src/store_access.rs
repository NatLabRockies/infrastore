//! The single seam between the CLI and the store backend.
//!
//! Today this opens the on-disk NetCDF + SQLite artifact directly. A future
//! `--endpoint` flag for the read-only gRPC server would be wired in here
//! without touching the command handlers.

use std::path::Path;

use time_series_store_core::{Store, create_store, open_store};

/// Open an existing store read-only. Errors if the file is missing.
pub fn open_readonly(path: &Path) -> Result<Store, String> {
    if !path.exists() {
        return Err(format!("store not found: {}", path.display()));
    }
    open_store(path, true).map_err(|e| e.to_string())
}

/// Open a writable store, creating it (and its SQLite sidecar) if absent.
pub fn open_writable(path: &Path) -> Result<Store, String> {
    if path.exists() {
        open_store(path, false).map_err(|e| e.to_string())
    } else {
        create_store(Some(path), false).map_err(|e| e.to_string())
    }
}
