//! The `add` command: load one or more series from a sidecar + CSV.

use std::path::Path;

use crate::{color, sidecar, store_access};

pub fn run(
    store_path: &Path,
    sidecar_path: &Path,
    csv_override: Option<&Path>,
) -> Result<(), String> {
    let sidecars = sidecar::load(sidecar_path)?;
    if csv_override.is_some() && sidecars.len() > 1 {
        return Err("--csv cannot be used with a [[series]] batch sidecar".to_string());
    }
    let base_dir = sidecar_path.parent();

    let mut requests = Vec::with_capacity(sidecars.len());
    for sc in &sidecars {
        requests.push(sc.to_add_request(base_dir, csv_override)?);
    }

    let mut store = store_access::open_writable(store_path)?;
    let keys = store
        .add_time_series_bulk(requests)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;

    for k in &keys {
        println!(
            "added {} '{}' (owner {})",
            k.time_series_type.as_str(),
            k.name,
            k.owner_uuid
        );
    }
    println!(
        "{}",
        color::header(&format!(
            "Added {} time series to {}.",
            keys.len(),
            store_path.display()
        ))
    );
    Ok(())
}
