//! Write-side maintenance commands: `remove`, `transform`, and `template`.

use std::path::Path;

use serde_json::json;

use crate::color;
use crate::confirm;
use crate::output::{Format, report};
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

/// `remove`: delete a single series, confirming first when interactive.
pub fn remove(
    store_path: &Path,
    selector: &SelectorArgs,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);

    if dry_run {
        let ts_type = meta.time_series_type.as_str();
        return report(
            format,
            || {
                json!({
                    "dry_run": true,
                    "would_remove": 1,
                    "name": meta.name,
                    "owner_id": meta.owner_id,
                    "time_series_type": ts_type,
                })
            },
            || {
                println!(
                    "Would remove {} '{}' (owner {}).",
                    ts_type, meta.name, meta.owner_id
                );
            },
        );
    }

    if !force
        && !confirm::ask(&format!(
            "Remove {} '{}' (owner {})? [y/N] ",
            meta.time_series_type.as_str(),
            meta.name,
            meta.owner_id
        ))?
    {
        return Ok(());
    }

    let mut store = store_access::open_writable(store_path)?;
    // The selector resolved one row to its catalog id, and a removal takes ids.
    store.remove_by_ids(&[key]).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || json!({ "removed": 1, "id": meta.id, "name": meta.name, "owner_id": meta.owner_id }),
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Removed '{}' (owner {}).",
                    meta.name, meta.owner_id
                ))
            );
        },
    )
}

/// `transform`: derive DeterministicSingleTimeSeries from stored SingleTimeSeries,
/// optionally scoped to an owner category and/or resolution.
pub fn transform(
    store_path: &Path,
    horizon: &str,
    interval: &str,
    owner_category: Option<&str>,
    resolution: Option<&str>,
    format: Format,
) -> Result<(), String> {
    let horizon = parse::parse_period(horizon)?;
    let interval = parse::parse_period(interval)?;
    let owner_category = owner_category
        .map(parse::parse_owner_category)
        .transpose()?;
    let resolution = resolution.map(parse::parse_period).transpose()?;
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .transform_single_time_series(
            horizon,
            interval,
            owner_category,
            resolution,
            Default::default(),
        )
        .map_err(|e| e.to_string())?
        .transformed;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || json!({ "transformed": n }),
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Transformed {n} SingleTimeSeries into DeterministicSingleTimeSeries."
                ))
            );
        },
    )
}

/// `remove --all`: remove every series matching the selector (may be several).
pub fn remove_all(
    store_path: &Path,
    selector: &SelectorArgs,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let filter = selector.to_filter()?;
    let matches = store
        .list_metadata(filter.clone())
        .map_err(|e| e.to_string())?;
    drop(store);
    if matches.is_empty() {
        // A zero count rather than nothing at all: a script that pipes this into
        // `jq .removed` should read 0, not choke on an empty document.
        return report(
            format,
            || json!({ "removed": 0 }),
            || {
                println!("{}", color::dim("No time series matched the selector."));
            },
        );
    }
    if dry_run {
        return report(
            format,
            || {
                json!({
                    "dry_run": true,
                    "would_remove": matches.len(),
                    "matches": matches.iter().map(identity_json).collect::<Vec<_>>(),
                })
            },
            || {
                println!("Would remove {} time series:", matches.len());
                for m in &matches {
                    println!(
                        "  - owner={} type={} name={}",
                        m.owner_id,
                        m.time_series_type.as_str(),
                        m.name
                    );
                }
            },
        );
    }
    if !force
        && !confirm::ask(&format!(
            "Remove {} time series matching the selector? [y/N] ",
            matches.len()
        ))?
    {
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store.remove_by_filter(filter).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || json!({ "removed": n }),
        || {
            println!("{}", color::header(&format!("Removed {n} time series.")));
        },
    )
}

/// The identifying triple of one series, for the `matches` list a `--dry-run`
/// reports under `-f json`.
fn identity_json(m: &infrastore_core::TimeSeriesMetadata) -> serde_json::Value {
    json!({
        "owner_id": m.owner_id,
        "time_series_type": m.time_series_type.as_str(),
        "name": m.name,
    })
}

/// `clear`: remove all series, or all for one owner, confirming when interactive.
pub fn clear(
    store_path: &Path,
    owner_id: Option<i64>,
    owner_category: Option<&str>,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let owner = match (owner_id, owner_category) {
        (Some(id), Some(cat)) => Some((id, parse::parse_owner_category(cat)?)),
        (None, None) => None,
        _ => {
            return Err("clear requires both --owner-id and --owner-category, or neither".into());
        }
    };
    if dry_run {
        let store = store_access::open_readonly(store_path)?;
        let mut filter = infrastore_core::ListFilter::new();
        if let Some((id, cat)) = owner {
            filter = filter.owner_id(id).owner_category(cat);
        }
        let n = store
            .list_metadata(filter)
            .map_err(|e| e.to_string())?
            .len();
        return report(
            format,
            || json!({ "dry_run": true, "would_clear": n }),
            || println!("Would clear {n} time series."),
        );
    }
    let scope = match owner {
        Some((id, _)) => format!("owner {id}"),
        None => "the entire store".to_string(),
    };
    if !force && !confirm::ask(&format!("Clear all time series for {scope}? [y/N] "))? {
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store.clear_time_series(owner).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || json!({ "cleared": n }),
        || {
            println!("{}", color::header(&format!("Cleared {n} time series.")));
        },
    )
}

/// `replace-owner`: reassign every series from one owner to another.
pub fn replace_owner(
    store_path: &Path,
    old: i64,
    new: i64,
    owner_category: &str,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let category = parse::parse_owner_category(owner_category)?;
    if dry_run {
        let store = store_access::open_readonly(store_path)?;
        let filter = infrastore_core::ListFilter::new()
            .owner_id(old)
            .owner_category(category);
        let n = store
            .list_metadata(filter)
            .map_err(|e| e.to_string())?
            .len();
        return report(
            format,
            || json!({ "dry_run": true, "would_reassign": n, "from": old, "to": new }),
            || println!("Would reassign {n} time series from owner {old} to {new}."),
        );
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .replace_owner(old, new, category)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || json!({ "reassigned": n, "from": old, "to": new }),
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Reassigned {n} time series from owner {old} to {new}."
                ))
            );
        },
    )
}

/// `copy`: copy the single series a selector resolves to onto another owner.
pub fn copy(
    store_path: &Path,
    selector: &SelectorArgs,
    dst_owner_id: i64,
    dst_owner_type: &str,
    new_name: Option<&str>,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);
    let dst_name = new_name.unwrap_or(&meta.name);
    if dry_run {
        return report(
            format,
            || {
                json!({
                    "dry_run": true,
                    "would_copy": 1,
                    "name": meta.name,
                    "owner_id": meta.owner_id,
                    "dst_owner_id": dst_owner_id,
                    "dst_owner_type": dst_owner_type,
                    "dst_name": dst_name,
                })
            },
            || {
                println!(
                    "Would copy '{}' (owner {}) to owner {dst_owner_id} ({dst_owner_type}) as '{dst_name}'.",
                    meta.name, meta.owner_id
                );
            },
        );
    }
    let mut store = store_access::open_writable(store_path)?;
    store
        .copy_time_series(key, dst_owner_id, dst_owner_type, new_name)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || {
            json!({
                "copied": 1,
                "dst_owner_id": dst_owner_id,
                "dst_owner_type": dst_owner_type,
                "dst_name": dst_name,
            })
        },
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Copied to owner {dst_owner_id} ({dst_owner_type})."
                ))
            );
        },
    )
}

/// `persist`: write the store to a new HDF5 + SQLite artifact.
///
/// Guarded, unlike every other write, for a specific reason: a `persist_to`
/// that fails partway may already have destroyed the destination, so
/// overwriting an existing artifact is the one operation here whose failure
/// mode loses data that was not otherwise at risk. An existing destination
/// therefore needs `--force` or an interactive `y`.
pub fn persist(
    store_path: &Path,
    dest: &Path,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let catalog = store_access::catalog_path(dest);
    // Both halves are one artifact, so either one existing counts as "there is
    // something here to lose".
    let existing: Vec<&Path> = [dest, catalog.as_path()]
        .into_iter()
        .filter(|p| p.exists())
        .collect();

    if dry_run {
        let overwriting: Vec<String> = existing.iter().map(|p| p.display().to_string()).collect();
        return report(
            format,
            || {
                json!({
                    "dry_run": true,
                    "would_write": [dest.display().to_string(), catalog.display().to_string()],
                    "overwriting": overwriting,
                })
            },
            || {
                println!("Would write {} and {}.", dest.display(), catalog.display());
                if !overwriting.is_empty() {
                    println!(
                        "{}",
                        color::dim(&format!("Overwriting: {}", overwriting.join(", ")))
                    );
                }
            },
        );
    }

    if !existing.is_empty() && !force {
        let listed = existing
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if !confirm::ask_strict(
            &format!(
                "{listed} already exist(s) and will be replaced. A failed save may leave \
                 neither the old nor the new artifact. Continue? [y/N] "
            ),
            "pass --force (or the global --yes) to overwrite it",
        )? {
            return Ok(());
        }
    }

    let mut store = store_access::open_writable(store_path)?;
    store.persist_to(dest).map_err(|e| e.to_string())?;
    report(
        format,
        || {
            json!({
                "persisted": true,
                "dest": dest.display().to_string(),
                "catalog": catalog.display().to_string(),
            })
        },
        || {
            println!(
                "{}",
                color::header(&format!("Persisted store to {}.", dest.display()))
            );
        },
    )
}

/// `init`: create an empty store with an explicit policy.
///
/// A store already springs into existence on the first `add`, so this exists for
/// what that cannot express: choosing a compression policy is only possible at
/// creation, and hanging that choice off `add` made it a flag that errors
/// whenever the store happens to exist already. Separating "create with a
/// policy" from "load data" also gives a load script somewhere to fail early.
pub fn init(
    store_path: &Path,
    compression: Option<infrastore_core::Compression>,
    catalog: store_access::CatalogChoice,
    format: Format,
) -> Result<(), String> {
    if store_path.exists() {
        return Err(format!(
            "{} already exists; init creates a new store",
            store_path.display()
        ));
    }
    let mut store = store_access::open_writable_with(store_path, compression, catalog)?;
    // Lands the catalog beside the arrays whichever mode was chosen, so the
    // command leaves a complete artifact rather than a half of one. Deferring
    // it to a later `infrastore persist` is not an option: "later" is another
    // process, and this process's catalog does not outlive it. See
    // `CatalogChoice`.
    store.persist_catalog().map_err(|e| e.to_string())?;
    report(
        format,
        || {
            json!({
                "created": store_path.display().to_string(),
                "catalog": catalog.to_string(),
            })
        },
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Created {} (catalog: {catalog}).",
                    store_path.display()
                ))
            );
        },
    )
}

/// `merge`: copy matching series from another store into this one.
///
/// The in-store form of `export` to a directory followed by `add` back, without
/// the round trip through CSV — and without the precision and metadata loss that
/// round trip implies, since the arrays move as bytes.
///
/// One asymmetry is worth knowing: a stored `DeterministicSingleTimeSeries`
/// reads back as a `Deterministic` (a storage-level view, by design), so it
/// lands in the destination as a real `Deterministic` rather than as a
/// transform of a source series. Merging the underlying `SingleTimeSeries` and
/// re-running `infrastore transform` reproduces the original arrangement.
pub fn merge(
    store_path: &Path,
    from: &Path,
    selector: &SelectorArgs,
    replace: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    if from == store_path {
        return Err("merge --from is the destination store itself".to_string());
    }
    let source = store_access::open_readonly(from)?;
    let metas = source
        .list_metadata(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    if metas.is_empty() {
        return report(
            format,
            || json!({ "merged": 0 }),
            || {
                println!("{}", color::dim("No time series matched the selector."));
            },
        );
    }
    if dry_run {
        return report(
            format,
            || {
                json!({
                    "dry_run": true,
                    "would_merge": metas.len(),
                    "matches": metas.iter().map(identity_json).collect::<Vec<_>>(),
                })
            },
            || {
                println!("Would merge {} time series:", metas.len());
                for m in &metas {
                    println!("  - {}", crate::fields::identity_line(m));
                }
            },
        );
    }

    let ids: Vec<infrastore_core::TimeSeriesId> = metas
        .iter()
        .map(crate::select::id_of)
        .collect::<Result<_, _>>()?;
    let datas = source
        .read_by_ids(&ids, infrastore_core::ReadWindow::full())
        .map_err(|e| e.to_string())?;
    drop(source);

    // A merge re-adds the source's rows, so the destination assigns them fresh
    // ids from its own stream — a source id may well already be taken there.
    let requests: Vec<infrastore_core::AddRequest> = metas
        .iter()
        .zip(datas)
        .map(|(m, data)| infrastore_core::AddRequest {
            owner_id: m.owner_id,
            owner_type: m.owner_type.clone(),
            owner_category: m.owner_category,
            data,
            features: m.features.clone(),
        })
        .collect();

    let mut store = store_access::open_writable(store_path)?;
    if replace {
        // Only what the destination already holds; a merge that brings in a
        // series the destination lacks is the normal case, not a failure.
        let filters: Vec<infrastore_core::ListFilter> =
            metas.iter().map(crate::select::exact_filter).collect();
        store_access::remove_existing(&mut store, &filters)?;
    }
    let n = store
        .add_time_series_bulk(requests)
        .map_err(|e| e.to_string())?
        .len();
    store.flush().map_err(|e| e.to_string())?;
    report(
        format,
        || {
            json!({
                "merged": n,
                "from": from.display().to_string(),
                "into": store_path.display().to_string(),
            })
        },
        || {
            println!(
                "{}",
                color::header(&format!(
                    "Merged {n} time series from {} into {}.",
                    from.display(),
                    store_path.display()
                ))
            );
        },
    )
}

/// `compact`: reclaim space; print the compaction report. Confirms first when
/// interactive — this rewrites the HDF5 file in place of the original, so it
/// must not run while anything else has the store open. `--force` bypasses.
pub fn compact(
    store_path: &Path,
    force: bool,
    format: crate::output::Format,
) -> Result<(), String> {
    if !force
        && !confirm::ask("Compact the store? This rewrites the .h5 file and replaces it. [y/N] ")?
    {
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let report = store.compact().map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    match format {
        f if f.is_json() => crate::output::print_value(
            f,
            &serde_json::json!({
            "slots_reclaimed": report.slots_reclaimed,
            "datasets_dropped": report.datasets_dropped,
            "feature_sets_reclaimed": report.feature_sets_reclaimed,
            "timestamp_sets_reclaimed": report.timestamp_sets_reclaimed,
                "bytes_reclaimed": report.bytes_reclaimed,
            }),
        )?,
        _ => {
            let headers = vec!["Metric".to_string(), "Value".to_string()];
            let rows = vec![
                vec![
                    "slots_reclaimed".to_string(),
                    report.slots_reclaimed.to_string(),
                ],
                vec![
                    "datasets_dropped".to_string(),
                    report.datasets_dropped.to_string(),
                ],
                vec![
                    "feature_sets_reclaimed".to_string(),
                    report.feature_sets_reclaimed.to_string(),
                ],
                vec![
                    "timestamp_sets_reclaimed".to_string(),
                    report.timestamp_sets_reclaimed.to_string(),
                ],
                vec![
                    "bytes_reclaimed".to_string(),
                    report.bytes_reclaimed.to_string(),
                ],
            ];
            if format == crate::output::Format::Csv {
                crate::output::display_csv_rows(&headers, &rows)?;
            } else {
                crate::output::display_table_dyn(&headers, &rows);
            }
        }
    }
    Ok(())
}

/// `template`: print an example descriptor for the given time-series type.
pub fn template(ts_type: &str) -> Result<(), String> {
    let kind = parse::parse_ts_type(ts_type)?;
    use infrastore_core::TimeSeriesType::*;
    let body = match kind {
        SingleTimeSeries => SINGLE,
        NonSequentialTimeSeries => NON_SEQUENTIAL,
        PersistentTimeSeries => PERSISTENT,
        Deterministic => DETERMINISTIC,
        Probabilistic => PROBABILISTIC,
        Scenarios => SCENARIOS,
        DeterministicSingleTimeSeries => {
            return Err(
                "DeterministicSingleTimeSeries is derived via `infrastore transform`, not a descriptor"
                    .to_string(),
            );
        }
    };
    crate::output::write_raw(body)
}

// Every template spells its `type`, `owner_category`, and durations exactly the
// way the CLI renders them back — `SingleTimeSeries`, `Component`, `PT1H` — so a
// descriptor generated here can be diffed and grepped against `list`, `info`, or
// `export -f json` output for the series it produced. The short spellings
// (`single`, `component`) are still accepted as input; they are just not what
// this hands you to start from.

const SINGLE: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "load",
  "type": "SingleTimeSeries",
  "element_type": "f64",
  "units": "MW",
  "application_data": "Profile",
  "csv": "load.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "features": {
    "model_year": 2030
  }
}
"#;

const NON_SEQUENTIAL: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "events",
  "type": "NonSequentialTimeSeries",
  "element_type": "f64",
  "units": "MW",
  "csv": "events.csv"
}
"#;

// A persistent series takes the same `timestamp,value` CSV as an irregular one;
// what changes is the read. `component_field` and `application_data` are shown
// because this is the type where they earn their keep: the field the curve
// varies, and the consumer's own policy for collapsing a step function.
const PERSISTENT: &str = r#"{
  "owner_id": 42,
  "owner_type": "ThermalStandard",
  "owner_category": "Component",
  "name": "gas_price",
  "type": "PersistentTimeSeries",
  "element_type": "f64",
  "units": "USD/MMBtu",
  "quantity_kind": "Currency",
  "component_field": "fuel_cost",
  "application_data": "{\"as_time_series\": false}",
  "csv": "gas_price.csv"
}
"#;

const DETERMINISTIC: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "load_forecast",
  "type": "Deterministic",
  "element_type": "f64",
  "units": "MW",
  "csv": "forecast.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT24H",
  "interval": "PT1H",
  "count": 7
}
"#;

const PROBABILISTIC: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "load_prob",
  "type": "Probabilistic",
  "element_type": "f64",
  "units": "MW",
  "csv": "prob.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT24H",
  "interval": "PT1H",
  "count": 7,
  "percentiles": [10.0, 50.0, 90.0]
}
"#;

const SCENARIOS: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "load_scenarios",
  "type": "Scenarios",
  "element_type": "f64",
  "units": "MW",
  "csv": "scenarios.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT24H",
  "interval": "PT1H",
  "count": 7,
  "scenario_count": 10
}
"#;
