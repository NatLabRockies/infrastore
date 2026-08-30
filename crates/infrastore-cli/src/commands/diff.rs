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

use infrastore_core::TimeSeriesMetadata;
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

/// One line of the report: what the two stores hold for a single identity.
struct Row {
    /// The lossless identity the two sides were paired on, kept to break ties in
    /// the output order. Never displayed — see [`identity_key`].
    identity: String,
    /// That same identity rendered for the reader.
    rendered: String,
    status: Status,
    left: Option<String>,
    right: Option<String>,
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

    let mut rows: Vec<Row> = Vec::new();
    for (id, (key, hash)) in &left {
        let (status, right_hash) = match right.get(id) {
            None => (Status::Removed, None),
            Some((_, other)) if other == hash => (Status::Same, Some(other.clone())),
            Some((_, other)) => (Status::Changed, Some(other.clone())),
        };
        rows.push(Row {
            identity: id.clone(),
            rendered: describe(key),
            status,
            left: Some(hash.clone()),
            right: right_hash,
        });
    }
    for (id, (key, hash)) in &right {
        if !left.contains_key(id) {
            rows.push(Row {
                identity: id.clone(),
                rendered: describe(key),
                status: Status::Added,
                left: None,
                right: Some(hash.clone()),
            });
        }
    }
    // By the rendering, so the report reads in a sensible order, then by the
    // identity, so two series that render alike still come out in the same order
    // every run — a diff has to be diffable itself.
    rows.sort_by(|a, b| {
        a.rendered
            .cmp(&b.rendered)
            .then_with(|| a.identity.cmp(&b.identity))
    });

    let counts = |s: Status| rows.iter().filter(|r| r.status == s).count();
    let differing = rows.len() - counts(Status::Same);
    let shown: Vec<&Row> = rows
        .iter()
        .filter(|r| all || r.status != Status::Same)
        .collect();

    let headers: Vec<String> = ["", "Status", "Series", "Left Hash", "Right Hash"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let table: Vec<Vec<String>> = shown
        .iter()
        .map(|r| {
            vec![
                r.status.marker().to_string(),
                r.status.as_str().to_string(),
                r.rendered.clone(),
                short(&r.left),
                short(&r.right),
            ]
        })
        .collect();

    match format {
        f if f.is_json() => {
            let items: Vec<Value> = shown
                .iter()
                .map(|r| {
                    json!({
                        "status": r.status.as_str(),
                        "series": r.rendered,
                        "left_data_hash": r.left,
                        "right_data_hash": r.right,
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

/// `identity_key -> (key, hex hash)` for one store.
fn load(
    path: &Path,
    filter: infrastore_core::ListFilter,
) -> Result<BTreeMap<String, (TimeSeriesMetadata, String)>, String> {
    let store = store_access::open_readonly(path)?;
    let rows = store.list_metadata(filter).map_err(|e| e.to_string())?;
    let mut out = BTreeMap::new();
    for key in rows {
        let hash = key.data_hash;
        out.insert(identity_key(&key), (key, fields::hash_hex(&hash)));
    }
    Ok(out)
}

/// The string two series are paired on: a lossless, unambiguous rendering of
/// the whole [`KeyIdentity`].
///
/// Distinct from [`describe`], and that separation is the point. `describe`
/// flattens features to `k=v` pairs joined by `,`, which is not injective — the
/// feature maps `{"a": "1,b=2"}` and `{"a": "1", "b": "2"}` both render as
/// `a=1,b=2`. Keying the comparison on that rendering collapsed two distinct
/// series into one map entry, so one of them vanished from the diff entirely and
/// two stores that genuinely differed could be reported as `0 changed` with exit
/// 0 — silently passing a CI gate built on that status. The same collision is
/// reachable through `name`, which can contain the literal text ` features=`.
///
/// `Debug` is used because it escapes strings and round-trips floats exactly
/// (`-0.0`, `inf` and `-inf` all print distinctly), which is what makes it
/// injective here. `serde_json` would not do: it writes every non-finite float
/// as `null`, so `+inf` and `-inf` features would collide again. The key never
/// leaves this process — it is built, compared, and dropped within one run — so
/// `Debug`'s lack of a cross-version stability guarantee does not matter.
fn identity_key(key: &TimeSeriesMetadata) -> String {
    format!(
        "{:?}",
        (
            key.owner_id,
            key.owner_category,
            key.time_series_type,
            &key.name,
            key.resolution,
            key.interval,
            &key.features,
        )
    )
}

/// A key rendered for the reader. Every identity field appears, so two series
/// that differ only by feature or interval are two rows rather than one spurious
/// `changed`. This is display only — the pairing is done on [`identity_key`],
/// because this rendering is ambiguous for features whose text contains `,`
/// or `=`.
fn describe(id: &TimeSeriesMetadata) -> String {
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
