//! The `diff` command: compare two stores at the catalog level.
//!
//! The regression check for "did this model run change what I expected".
//! Content addressing makes it cheap: two series hold the same numbers exactly
//! when they carry the same `data_hash`, so the whole comparison is a set
//! operation over `(identity, hash)` pairs and neither store's arrays are read.
//!
//! Scoped deliberately to the catalog. A `changed` row says the bytes differ,
//! not how — `infrastore get` on each side is the next step, and keeping the
//! diff itself hash-only is what lets it run over a multi-GB pair in the time a
//! catalog scan takes.

use std::collections::BTreeMap;
use std::path::Path;

use infrastore_core::TimeSeriesKey;
use serde_json::{Value, json};

use crate::color;
use crate::fields;
use crate::output::{self, Format};
use crate::select::SelectorArgs;
use crate::store_access;

/// What a diff found for one identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Added,
    Removed,
    Changed,
    Same,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Removed => "removed",
            Status::Changed => "changed",
            Status::Same => "same",
        }
    }

    /// The `git diff` marker, so a table scans the way a reader expects.
    fn marker(self) -> &'static str {
        match self {
            Status::Added => "+",
            Status::Removed => "-",
            Status::Changed => "~",
            Status::Same => " ",
        }
    }
}

pub fn run(
    left_path: &Path,
    right_path: &Path,
    selector: &SelectorArgs,
    all: bool,
    format: Format,
) -> Result<(), String> {
    let filter = selector.to_filter()?;
    let left = load(left_path, filter.clone())?;
    let right = load(right_path, filter)?;

    // BTreeMap keyed by the rendered identity: `KeyIdentity` is hashable but has
    // no total order, and a diff has to come out in the same order every run to
    // be diffable itself.
    let mut rows: Vec<(String, Status, Option<String>, Option<String>)> = Vec::new();
    for (id, (key, hash)) in &left {
        match right.get(id) {
            None => rows.push((describe(key), Status::Removed, Some(hash.clone()), None)),
            Some((_, other)) if other == hash => rows.push((
                describe(key),
                Status::Same,
                Some(hash.clone()),
                Some(other.clone()),
            )),
            Some((_, other)) => rows.push((
                describe(key),
                Status::Changed,
                Some(hash.clone()),
                Some(other.clone()),
            )),
        }
    }
    for (id, (key, hash)) in &right {
        if !left.contains_key(id) {
            rows.push((describe(key), Status::Added, None, Some(hash.clone())));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let counts = |s: Status| rows.iter().filter(|r| r.1 == s).count();
    let differing = rows.len() - counts(Status::Same);
    let shown: Vec<_> = rows.iter().filter(|r| all || r.1 != Status::Same).collect();

    let headers: Vec<String> = ["", "Status", "Series", "Left Hash", "Right Hash"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let table: Vec<Vec<String>> = shown
        .iter()
        .map(|(desc, status, l, r)| {
            vec![
                status.marker().to_string(),
                status.as_str().to_string(),
                desc.clone(),
                short(l),
                short(r),
            ]
        })
        .collect();

    match format {
        f if f.is_json() => {
            let items: Vec<Value> = shown
                .iter()
                .map(|(desc, status, l, r)| {
                    json!({
                        "status": status.as_str(),
                        "series": desc,
                        "left_data_hash": l,
                        "right_data_hash": r,
                    })
                })
                .collect();
            output::print_value(
                f,
                &json!({
                    "left": left_path.display().to_string(),
                    "right": right_path.display().to_string(),
                    "added": counts(Status::Added),
                    "removed": counts(Status::Removed),
                    "changed": counts(Status::Changed),
                    "same": counts(Status::Same),
                    "items": items,
                }),
            )?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table)?,
        _ => {
            output::display_table_dyn(&headers, &table);
            println!(
                "{}",
                color::header(&format!(
                    "{} added, {} removed, {} changed, {} identical.",
                    counts(Status::Added),
                    counts(Status::Removed),
                    counts(Status::Changed),
                    counts(Status::Same),
                ))
            );
        }
    }

    // Nonzero when the stores differ, so `diff` drops into a CI gate the same
    // way `verify` does. A read or open failure is also nonzero, but with a
    // message on stderr.
    if differing > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// `identity -> (key, hex hash)` for one store.
fn load(
    path: &Path,
    filter: infrastore_core::ListFilter,
) -> Result<BTreeMap<String, (TimeSeriesKey, String)>, String> {
    let store = store_access::open_readonly(path)?;
    let rows = store
        .list_keys_with_hash(filter)
        .map_err(|e| e.to_string())?;
    let mut out = BTreeMap::new();
    for (key, hash) in rows {
        out.insert(describe(&key), (key, fields::hash_hex(&hash)));
    }
    Ok(out)
}

/// A key rendered as the string that is both its comparison identity and its
/// display. Every identity field appears, so two series that differ only by
/// feature or interval are two rows rather than one spurious `changed`.
fn describe(key: &TimeSeriesKey) -> String {
    let id = key.identity();
    format!(
        "owner={} category={} type={} name={} resolution={} interval={} features={}",
        id.owner_id,
        id.owner_category.as_str(),
        id.time_series_type.as_str(),
        id.name,
        fields::opt_period(id.resolution),
        fields::opt_period(id.interval),
        fields::features_str(&id.features),
    )
}

fn short(hash: &Option<String>) -> String {
    match hash {
        Some(h) => h.chars().take(fields::SHORT_HASH_LEN).collect(),
        None => "-".to_string(),
    }
}
