//! Discovery commands: `names`, `owner-types`, `owners`, and `exists`.
//!
//! The step before a selector. `stats` says a store holds 5000 series and
//! `list` shows the ones matching a filter, but writing that filter means
//! already knowing which names, owner types, and owner ids exist. These are the
//! projections the core keeps for exactly that (`list_names`,
//! `list_owner_types`, `list_owner_ids`), each scoped by the same
//! [`SelectorArgs`] every other read command takes, so narrowing composes:
//! `names --owner-id 42` asks what that one component carries.

use std::path::Path;

use serde_json::json;

use crate::color;
use crate::output::{self, Format};
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

/// `names`: distinct series names matching the selector.
pub fn names(store_path: &Path, selector: &SelectorArgs, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let names = store
        .list_names(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    print_column("Name", &names, format)
}

/// `owner-types`: distinct owner types matching the selector.
pub fn owner_types(
    store_path: &Path,
    selector: &SelectorArgs,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let types = store
        .list_owner_types(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    print_column("Owner Type", &types, format)
}

/// `owners`: distinct owner ids that have a time series.
///
/// The core's projection is keyed by owner category rather than by a general
/// filter, so this command reads `--owner-category` (defaulting to `Component`,
/// which is what the overwhelming majority of a store's owners are), `--type`,
/// and `--resolution` from the selector and rejects the rest rather than
/// silently ignoring them.
pub fn owners(store_path: &Path, selector: &SelectorArgs, format: Format) -> Result<(), String> {
    for (flag, set) in [
        ("--owner-id", selector.owner_id.is_some()),
        ("--name", selector.name.is_some()),
        ("--name-glob", selector.name_glob.is_some()),
        ("--feature", !selector.feature.is_empty()),
    ] {
        if set {
            return Err(format!(
                "owners lists owner ids, so it takes only --owner-category, --type, and \
                 --resolution; {flag} does not narrow it. Use `list` for a full filter."
            ));
        }
    }
    let category = match &selector.owner_category {
        Some(c) => parse::parse_owner_category(c)?,
        None => infrastore_core::OwnerCategory::Component,
    };
    let ts_type = selector
        .ts_type
        .as_deref()
        .map(parse::parse_ts_type)
        .transpose()?;
    let resolution = selector
        .resolution
        .as_deref()
        .map(parse::parse_period)
        .transpose()?;

    let store = store_access::open_readonly(store_path)?;
    let ids = store
        .list_owner_ids(category, ts_type, resolution)
        .map_err(|e| e.to_string())?;
    let cells: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    match format {
        f if f.is_json() => output::print_value(f, &json!({ "owner_ids": ids })),
        _ => print_column("Owner", &cells, format),
    }
}

/// `exists`: whether anything matches, as an exit status.
///
/// The scripting primitive over `has_any_time_series` — with a selector that
/// pins a full identity it answers the keyed question too. Exit 0 means found,
/// 1 means not found, so it drops into `if infrastore exists ...; then`. The
/// printed `true`/`false` is for humans; a script should read the status, since
/// every other failure also exits nonzero but with a message on stderr.
pub fn exists(store_path: &Path, selector: &SelectorArgs, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let found = store
        .has_any_time_series(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    match format {
        f if f.is_json() => output::print_value(f, &json!({ "exists": found }))?,
        Format::Csv => {
            output::display_csv_rows(&["exists".to_string()], &[vec![found.to_string()]])?
        }
        _ => println!("{found}"),
    }
    if !found {
        std::process::exit(1);
    }
    Ok(())
}

/// A single-column result in whichever format was asked for.
fn print_column(header: &str, values: &[String], format: Format) -> Result<(), String> {
    let headers = vec![header.to_string()];
    let rows: Vec<Vec<String>> = values.iter().map(|v| vec![v.clone()]).collect();
    match format {
        f if f.is_json() => output::print_items(f, values),
        Format::Csv => output::display_csv_rows(&headers, &rows),
        _ => {
            if values.is_empty() {
                println!("{}", color::dim("(no results)"));
            } else {
                output::display_table_dyn(&headers, &rows);
            }
            Ok(())
        }
    }
}
