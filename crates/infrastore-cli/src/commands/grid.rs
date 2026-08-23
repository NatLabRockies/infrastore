//! The `grid` command: N series as N columns against one shared time axis.
//!
//! This is the CLI surface for the core's columnar reader
//! ([`infrastore_core::StaticReader`]) and the read-direction inverse of the
//! wide-CSV ingest in [`crate::descriptor::ColumnLayout::Wide`]: `grid` writes
//! `timestamp,gen_001,gen_002,...` and `add` reads it back.
//!
//! A reader spans exactly one timeline, which is what makes the columns line up
//! row by row without a presence mask. For `SingleTimeSeries` that means one
//! resolution (so `--resolution` is required); for `NonSequentialTimeSeries` it
//! means one shared timestamp vector. The core reports a divergent selection as
//! an error rather than padding it, and that error is passed through unchanged.
//!
//! One timeline also means one *spelling* for it, so a selection mixing
//! wall-clock series with instant-bearing ones is refused on the same terms.
//! `--spelling zoned|zoneless` narrows it to one of the two groups.

use std::path::Path;

use chrono::{DateTime, Utc};
use infrastore_core::{StaticReader, Store, TimeSeriesKey};
use serde_json::{Value, json};

use crate::color;
use crate::csv_io;
use crate::output::{self, Format};
use crate::select::SelectorArgs;
use crate::store_access;

/// Rows a table shows before truncating, matching `get`.
const DEFAULT_LIMIT: usize = 50;

/// How a grid column is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColumnLabel {
    /// Bare owner id when every column shares one series name, else
    /// `name@owner`. The bare form is what makes a `grid` CSV re-readable by
    /// `add --owner-id-from header`.
    #[default]
    Auto,
    /// Always the bare owner id.
    Owner,
    /// Always `name@owner`.
    Full,
}

pub fn run(
    store_path: &Path,
    selector: &SelectorArgs,
    time_range: Option<&str>,
    limit: Option<usize>,
    full: bool,
    label: ColumnLabel,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let mut reader = store
        .build_static_reader(selector.to_filter()?)
        .map_err(|e| e.to_string())?;

    let headers = column_headers(&reader, label);
    if headers.len() == 1 {
        return Err("no time series matched the selector".to_string());
    }

    let range = crate::parse::parse_time_range(time_range)?;
    // Decision 8 again: a bound has to be spelled the way the thing it slices
    // is. Every other ranged read gets this for free by handing the range to the
    // core, which checks it once in `materialize_time_series`; `grid` filters the
    // reader's own axis here instead, so without this the check is simply
    // skipped — `get` refuses a wall-clock bound against an instant-bearing
    // series while `grid` quietly answered it.
    if let Some(r) = range {
        r.check_against(reader.time_reference(), "this reader's timeline")
            .map_err(|e| e.to_string())?;
    }
    let all: Vec<DateTime<Utc>> = reader
        .timestamps()
        .filter(|t| match range {
            Some(r) => *t >= r.start && *t < r.end,
            None => true,
        })
        .collect();
    // `--limit` and `--full` bound a display, so — exactly as in `get` — they
    // apply to the table and nowhere else. A `grid -f csv` pipe is read straight
    // back by `add`, and an explicit `--limit` used to shorten it silently:
    // the truncated file is still a valid wide CSV, so the missing hours only
    // surface much later as a series that ends early. Slice a pipe with
    // `--time-range` instead.
    let max = match (format, full, limit) {
        (Format::Table, false, Some(n)) => n,
        (Format::Table, false, None) => DEFAULT_LIMIT,
        _ => all.len(),
    };
    let shown = all.len().min(max);

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(shown);
    for at in all.iter().take(shown) {
        rows.push(read_row(&store, &mut reader, *at)?);
    }

    match format {
        f if f.is_json() => {
            let items: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "timestamp": r[0],
                        "values": &r[1..],
                    })
                })
                .collect();
            output::print_value(
                f,
                &json!({
                    "columns": &headers[1..],
                    "rows": items,
                }),
            )?;
        }
        Format::Csv => output::display_csv_rows(&headers, &rows)?,
        _ => {
            output::display_table_dyn(&headers, &rows);
            if all.len() > shown {
                println!(
                    "{}",
                    color::dim(&format!(
                        "... {} more rows (use --full, --limit, or -f csv)",
                        all.len() - shown
                    ))
                );
            }
        }
    }
    Ok(())
}

/// `timestamp` plus one header per column, in the reader's own column order.
///
/// Groups are ordered by `(dtype, element_shape)` and each group's keys keep
/// their build order, so the layout is stable across runs of the same
/// selector — which is what lets two grid exports be diffed.
fn column_headers(reader: &StaticReader, label: ColumnLabel) -> Vec<String> {
    let keys: Vec<&TimeSeriesKey> = reader
        .groups()
        .iter()
        .flat_map(|g| g.keys().iter())
        .collect();
    let one_name = keys
        .first()
        .map(|k| keys.iter().all(|o| o.name() == k.name()))
        .unwrap_or(false);
    let bare = match label {
        ColumnLabel::Owner => true,
        ColumnLabel::Full => false,
        ColumnLabel::Auto => one_name,
    };
    let name_of = |k: &TimeSeriesKey| {
        if bare {
            k.owner_id().to_string()
        } else {
            format!("{}@{}", k.name(), k.owner_id())
        }
    };

    let mut headers = vec!["timestamp".to_string()];
    for group in reader.groups() {
        let per_step: usize = group.element_shape().iter().product::<usize>().max(1);
        for key in group.keys() {
            if per_step <= 1 {
                headers.push(name_of(key));
            } else {
                headers.extend((0..per_step).map(|e| format!("{}[{e}]", name_of(key))));
            }
        }
    }
    headers
}

/// One row: the timestamp, then every column's value at it.
fn read_row(
    store: &Store,
    reader: &mut StaticReader,
    at: DateTime<Utc>,
) -> Result<Vec<String>, String> {
    store.static_read(reader, at).map_err(|e| e.to_string())?;
    // The axis is spelled the way the cohort's own series are, so a `grid -f
    // csv` pipe reads straight back into `add` under the same reference it came
    // out under. A cohort that mixes zoneless with the rest never builds a
    // reader at all, so there is always exactly one spelling here.
    let mut row = vec![crate::fields::render_timestamp(at, reader.time_reference())];
    for group in reader.groups() {
        row.extend(csv_io::bytes_to_strings(group.dtype(), group.values()));
    }
    Ok(row)
}
