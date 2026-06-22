//! The `add` command: load one or more series from a descriptor JSON + CSV.

use std::path::Path;

use crate::{color, descriptor, store_access};

pub fn run(
    store_path: &Path,
    descriptor_path: &Path,
    csv_override: Option<&Path>,
) -> Result<(), String> {
    let descriptors = descriptor::load(descriptor_path)?;
    if csv_override.is_some() && descriptors.len() > 1 {
        return Err("--csv cannot be used with an array descriptor".to_string());
    }
    let base_dir = descriptor_path.parent();

    let mut requests = Vec::with_capacity(descriptors.len());
    for desc in &descriptors {
        requests.push(desc.to_add_request(base_dir, csv_override)?);
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
            k.owner_id
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
