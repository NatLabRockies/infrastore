//! The association catalogs: `attributes` / `links` read them, `attach` /
//! `detach` / `link` / `unlink` / `reassign` write them.
//!
//! Both catalogs are independent of time series and of each other, so these
//! commands take no time-series selector.
//!
//! The write half exists because the CLI is a store-authoring tool — `init`,
//! `add`, `merge`, `persist` all say so — and a store whose association half
//! could only be filled in from Rust, Python, or Julia was not one the CLI could
//! finish building. The store holds only the *relationship*; the components and
//! attributes themselves live in the consumer's object graph, which is why the
//! flags here are bare ids and type names rather than objects.

use std::path::Path;

use infrastore_core::{
    ParentChildAssociation, ParentChildFilter, SupplementalAttributeAssociation,
    SupplementalAttributeFilter,
};
use serde_json::{Value, json};

use crate::color;
use crate::confirm;
use crate::output::{self, Format, report};
use crate::store_access;

/// `attributes`: component <-> supplemental-attribute attachments.
pub fn attributes(
    store_path: &Path,
    component_id: Option<i64>,
    attribute_id: Option<i64>,
    component_type: Option<&str>,
    attribute_type: Option<&str>,
    summary: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;

    if summary {
        let rows = store
            .supplemental_attribute_summary()
            .map_err(|e| e.to_string())?;
        let headers = vec![
            "Component Type".to_string(),
            "Attribute Type".to_string(),
            "Attachments".to_string(),
        ];
        let table: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.component_type.clone(),
                    r.attribute_type.clone(),
                    r.count.to_string(),
                ]
            })
            .collect();
        return match format {
            f if f.is_json() => {
                let items: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        json!({
                            "component_type": r.component_type,
                            "attribute_type": r.attribute_type,
                            "count": r.count,
                        })
                    })
                    .collect();
                output::print_items(f, &items)
            }
            Format::Csv => output::display_csv_rows(&headers, &table),
            _ => {
                println!("{}", color::header("Supplemental attributes by type"));
                output::display_table_dyn(&headers, &table);
                Ok(())
            }
        };
    }

    let filter = attribute_filter(component_id, attribute_id, component_type, attribute_type);

    let rows = store
        .list_supplemental_attribute_associations(&filter)
        .map_err(|e| e.to_string())?;
    let headers = vec![
        "Component".to_string(),
        "Component Type".to_string(),
        "Attribute".to_string(),
        "Attribute Type".to_string(),
    ];
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.component_id.to_string(),
                r.component_type.clone(),
                r.attribute_id.to_string(),
                r.attribute_type.clone(),
            ]
        })
        .collect();
    match format {
        f if f.is_json() => {
            let items: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "component_id": r.component_id,
                        "component_type": r.component_type,
                        "attribute_id": r.attribute_id,
                        "attribute_type": r.attribute_type,
                    })
                })
                .collect();
            output::print_items(f, &items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table)?,
        _ => output::display_table_dyn(&headers, &table),
    }
    Ok(())
}

/// `links`: directed parent -> child edges between components.
pub fn links(
    store_path: &Path,
    parent_id: Option<i64>,
    child_id: Option<i64>,
    parent_type: Option<&str>,
    child_type: Option<&str>,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let filter = link_filter(parent_id, child_id, parent_type, child_type);

    let rows = store
        .list_parent_child_associations(&filter)
        .map_err(|e| e.to_string())?;
    let headers = vec![
        "Parent".to_string(),
        "Parent Type".to_string(),
        "Child".to_string(),
        "Child Type".to_string(),
    ];
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.parent_id.to_string(),
                r.parent_type.clone(),
                r.child_id.to_string(),
                r.child_type.clone(),
            ]
        })
        .collect();
    match format {
        f if f.is_json() => {
            let items: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "parent_id": r.parent_id,
                        "parent_type": r.parent_type,
                        "child_id": r.child_id,
                        "child_type": r.child_type,
                    })
                })
                .collect();
            output::print_items(f, &items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table)?,
        _ => output::display_table_dyn(&headers, &table),
    }
    Ok(())
}

// --- filters shared by the read and write halves ---------------------------

/// The core takes a list of concrete type names; the CLI exposes one, which is
/// the common case and keeps the flag a plain string.
fn attribute_filter(
    component_id: Option<i64>,
    attribute_id: Option<i64>,
    component_type: Option<&str>,
    attribute_type: Option<&str>,
) -> SupplementalAttributeFilter {
    let mut filter = SupplementalAttributeFilter::new();
    if let Some(id) = component_id {
        filter = filter.component_id(id);
    }
    if let Some(id) = attribute_id {
        filter = filter.attribute_id(id);
    }
    if let Some(t) = component_type {
        filter = filter.component_types(vec![t.to_string()]);
    }
    if let Some(t) = attribute_type {
        filter = filter.attribute_types(vec![t.to_string()]);
    }
    filter
}

fn link_filter(
    parent_id: Option<i64>,
    child_id: Option<i64>,
    parent_type: Option<&str>,
    child_type: Option<&str>,
) -> ParentChildFilter {
    let mut filter = ParentChildFilter::new();
    if let Some(id) = parent_id {
        filter = filter.parent_id(id);
    }
    if let Some(id) = child_id {
        filter = filter.child_id(id);
    }
    if let Some(t) = parent_type {
        filter = filter.parent_types(vec![t.to_string()]);
    }
    if let Some(t) = child_type {
        filter = filter.child_types(vec![t.to_string()]);
    }
    filter
}

// --- writes ----------------------------------------------------------------

/// The four fields of one attachment, as flags.
pub struct AttachArgs<'a> {
    pub component_id: Option<i64>,
    pub component_type: Option<&'a str>,
    pub attribute_id: Option<i64>,
    pub attribute_type: Option<&'a str>,
    /// A `component_id,component_type,attribute_id,attribute_type` CSV.
    pub from: Option<&'a Path>,
    pub dry_run: bool,
    pub format: Format,
}

/// `attach`: attach supplemental attributes to components.
///
/// One attachment from flags, or a whole table from `--from`. The bulk form
/// goes through the core's all-or-nothing batch insert, so a duplicate anywhere
/// in the file leaves the catalog exactly as it was rather than half-imported.
pub fn attach(store_path: &Path, args: &AttachArgs<'_>) -> Result<(), String> {
    let rows = match args.from {
        Some(path) => read_assoc_csv(path, ATTACH_COLUMNS)?
            .into_iter()
            .map(|r| SupplementalAttributeAssociation {
                component_id: r.0,
                component_type: r.1,
                attribute_id: r.2,
                attribute_type: r.3,
            })
            .collect::<Vec<_>>(),
        None => vec![SupplementalAttributeAssociation {
            component_id: require_id(args.component_id, "--component-id")?,
            component_type: require_type(args.component_type, "--component-type")?,
            attribute_id: require_id(args.attribute_id, "--attribute-id")?,
            attribute_type: require_type(args.attribute_type, "--attribute-type")?,
        }],
    };
    if args.dry_run {
        return report(
            args.format,
            json!({
                "dry_run": true,
                "would_attach": rows.len(),
                "attachments": rows.iter().map(|r| json!({
                    "component_id": r.component_id,
                    "component_type": r.component_type,
                    "attribute_id": r.attribute_id,
                    "attribute_type": r.attribute_type,
                })).collect::<Vec<_>>(),
            }),
            || {
                println!("Would attach {} supplemental attribute(s):", rows.len());
                for r in &rows {
                    println!(
                        "  - component {} ({}) <- attribute {} ({})",
                        r.component_id, r.component_type, r.attribute_id, r.attribute_type
                    );
                }
            },
        );
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .add_supplemental_attribute_associations(rows)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(args.format, json!({ "attached": n }), || {
        println!(
            "{}",
            color::header(&format!("Attached {n} supplemental attribute(s)."))
        );
    })
}

/// `detach`: remove every attachment matching the filter.
///
/// A bare `detach` would empty the whole catalog, so it insists on at least one
/// narrowing flag — `--all` is how you say you meant it.
#[allow(clippy::too_many_arguments)]
pub fn detach(
    store_path: &Path,
    component_id: Option<i64>,
    attribute_id: Option<i64>,
    component_type: Option<&str>,
    attribute_type: Option<&str>,
    all: bool,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let filter = attribute_filter(component_id, attribute_id, component_type, attribute_type);
    let narrowed = component_id.is_some()
        || attribute_id.is_some()
        || component_type.is_some()
        || attribute_type.is_some();
    if !narrowed && !all {
        return Err(
            "detach with no filter would remove every attachment; pass --all to mean that, \
             or narrow with --component-id/--attribute-id/--component-type/--attribute-type"
                .to_string(),
        );
    }

    let store = store_access::open_readonly(store_path)?;
    let matched = store
        .count_supplemental_attribute_associations(&filter)
        .map_err(|e| e.to_string())?;
    drop(store);
    if dry_run {
        return report(
            format,
            json!({ "dry_run": true, "would_detach": matched }),
            || println!("Would detach {matched} supplemental attribute attachment(s)."),
        );
    }
    if matched == 0 {
        return report(format, json!({ "detached": 0 }), || {
            println!("{}", color::dim("No attachments matched the filter."));
        });
    }
    if !force && !confirm::ask(&format!("Detach {matched} attachment(s)? [y/N] "))? {
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .remove_supplemental_attribute_associations(&filter)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(format, json!({ "detached": n }), || {
        println!("{}", color::header(&format!("Detached {n} attachment(s).")));
    })
}

/// The four fields of one directed edge, as flags.
pub struct LinkArgs<'a> {
    pub parent_id: Option<i64>,
    pub parent_type: Option<&'a str>,
    pub child_id: Option<i64>,
    pub child_type: Option<&'a str>,
    /// A `parent_id,parent_type,child_id,child_type` CSV.
    pub from: Option<&'a Path>,
    pub dry_run: bool,
    pub format: Format,
}

/// `link`: add directed parent -> child component edges.
pub fn link(store_path: &Path, args: &LinkArgs<'_>) -> Result<(), String> {
    let rows = match args.from {
        Some(path) => read_assoc_csv(path, LINK_COLUMNS)?
            .into_iter()
            .map(|r| ParentChildAssociation {
                parent_id: r.0,
                parent_type: r.1,
                child_id: r.2,
                child_type: r.3,
            })
            .collect::<Vec<_>>(),
        None => vec![ParentChildAssociation {
            parent_id: require_id(args.parent_id, "--parent-id")?,
            parent_type: require_type(args.parent_type, "--parent-type")?,
            child_id: require_id(args.child_id, "--child-id")?,
            child_type: require_type(args.child_type, "--child-type")?,
        }],
    };
    if args.dry_run {
        return report(
            args.format,
            json!({
                "dry_run": true,
                "would_link": rows.len(),
                "links": rows.iter().map(|r| json!({
                    "parent_id": r.parent_id,
                    "parent_type": r.parent_type,
                    "child_id": r.child_id,
                    "child_type": r.child_type,
                })).collect::<Vec<_>>(),
            }),
            || {
                println!("Would add {} link(s):", rows.len());
                for r in &rows {
                    println!(
                        "  - {} ({}) -> {} ({})",
                        r.parent_id, r.parent_type, r.child_id, r.child_type
                    );
                }
            },
        );
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .add_parent_child_associations(rows)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(args.format, json!({ "linked": n }), || {
        println!("{}", color::header(&format!("Added {n} link(s).")));
    })
}

/// `unlink`: remove every edge matching the filter.
#[allow(clippy::too_many_arguments)]
pub fn unlink(
    store_path: &Path,
    parent_id: Option<i64>,
    child_id: Option<i64>,
    parent_type: Option<&str>,
    child_type: Option<&str>,
    all: bool,
    force: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    let filter = link_filter(parent_id, child_id, parent_type, child_type);
    let narrowed =
        parent_id.is_some() || child_id.is_some() || parent_type.is_some() || child_type.is_some();
    if !narrowed && !all {
        return Err(
            "unlink with no filter would remove every edge; pass --all to mean that, or \
             narrow with --parent-id/--child-id/--parent-type/--child-type"
                .to_string(),
        );
    }

    let store = store_access::open_readonly(store_path)?;
    let matched = store
        .count_parent_child_associations(&filter)
        .map_err(|e| e.to_string())?;
    drop(store);
    if dry_run {
        return report(
            format,
            json!({ "dry_run": true, "would_unlink": matched }),
            || println!("Would remove {matched} link(s)."),
        );
    }
    if matched == 0 {
        return report(format, json!({ "unlinked": 0 }), || {
            println!("{}", color::dim("No links matched the filter."));
        });
    }
    if !force && !confirm::ask(&format!("Remove {matched} link(s)? [y/N] "))? {
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .remove_parent_child_associations(&filter)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    report(format, json!({ "unlinked": n }), || {
        println!("{}", color::header(&format!("Removed {n} link(s).")));
    })
}

/// `reassign`: move a component's associations from one id to another.
///
/// The association counterpart of `replace-owner`, which moves time series. The
/// two are separate commands because they move different things: a component
/// that has been renumbered usually needs both, and running them separately is
/// what makes it visible that both happened.
pub fn reassign(
    store_path: &Path,
    old: i64,
    new: i64,
    attributes: bool,
    links: bool,
    dry_run: bool,
    format: Format,
) -> Result<(), String> {
    // Neither flag means both — the usual reason to reassign is that a
    // component was renumbered, and that moves everything about it.
    let (do_attributes, do_links) = match (attributes, links) {
        (false, false) => (true, true),
        pair => pair,
    };
    if dry_run {
        let store = store_access::open_readonly(store_path)?;
        let mut counts = ReassignCounts::default();
        if do_attributes {
            counts.attachments = Some(
                store
                    .count_supplemental_attribute_associations(
                        &SupplementalAttributeFilter::new().component_id(old),
                    )
                    .map_err(|e| e.to_string())?,
            );
        }
        if do_links {
            let as_parent = store
                .count_parent_child_associations(&ParentChildFilter::new().parent_id(old))
                .map_err(|e| e.to_string())?;
            let as_child = store
                .count_parent_child_associations(&ParentChildFilter::new().child_id(old))
                .map_err(|e| e.to_string())?;
            counts.links = Some(as_parent + as_child);
        }
        let mut doc = counts.to_json(old, new);
        doc["dry_run"] = json!(true);
        let prose = counts.prose();
        return report(format, doc, || {
            println!("Would reassign {prose} from component {old} to {new}.");
        });
    }

    let mut store = store_access::open_writable(store_path)?;
    let mut counts = ReassignCounts::default();
    if do_attributes {
        let n = store
            .replace_supplemental_attribute_component_id(old, new)
            .map_err(|e| e.to_string())?;
        counts.attachments = Some(n as i64);
    }
    if do_links {
        let n = store
            .replace_parent_child_component_id(old, new)
            .map_err(|e| e.to_string())?;
        counts.links = Some(n as i64);
    }
    store.flush().map_err(|e| e.to_string())?;
    let prose = counts.prose();
    report(format, counts.to_json(old, new), || {
        println!(
            "{}",
            color::header(&format!(
                "Reassigned {prose} from component {old} to {new}."
            ))
        );
    })
}

/// What a `reassign` touched, per catalog.
///
/// `None` means the run was scoped away from that catalog by `--attributes` /
/// `--links`, which is not the same as having found nothing there — so the JSON
/// omits the key entirely rather than reporting a zero the caller would read as
/// "checked, empty".
///
/// Counts are `i64` because that is what the catalog's `count_*` calls return;
/// the `replace_*` calls hand back a `usize` row count that is converted on the
/// way in.
#[derive(Default)]
struct ReassignCounts {
    attachments: Option<i64>,
    links: Option<i64>,
}

impl ReassignCounts {
    fn to_json(&self, old: i64, new: i64) -> Value {
        let mut doc = json!({ "from": old, "to": new });
        if let Some(n) = self.attachments {
            doc["attachments"] = json!(n);
        }
        if let Some(n) = self.links {
            doc["links"] = json!(n);
        }
        doc
    }

    fn prose(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = self.attachments {
            parts.push(format!("{n} attachment(s)"));
        }
        if let Some(n) = self.links {
            parts.push(format!("{n} link(s)"));
        }
        parts.join(" and ")
    }
}

// --- bulk import -----------------------------------------------------------

const ATTACH_COLUMNS: [&str; 4] = [
    "component_id",
    "component_type",
    "attribute_id",
    "attribute_type",
];
const LINK_COLUMNS: [&str; 4] = ["parent_id", "parent_type", "child_id", "child_type"];

/// Read an `id,type,id,type` association CSV, checking the header names.
///
/// The header is mandatory and verified rather than assumed, because the four
/// columns are two interchangeable-looking `(id, type)` pairs: a file with the
/// pairs swapped would import cleanly and silently invert every relationship.
/// The column names are exactly the ones `attributes -f csv` / `links -f csv`
/// would write in lowercase, so an export can be edited and fed back in.
fn read_assoc_csv(
    path: &Path,
    expected: [&str; 4],
) -> Result<Vec<(i64, String, i64, String)>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    let header: Vec<String> = reader
        .headers()
        .map_err(|e| format!("reading the header of {}: {e}", path.display()))?
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    if header != expected {
        return Err(format!(
            "{}: expected the header `{}` (found `{}`)",
            path.display(),
            expected.join(","),
            header.join(",")
        ));
    }

    let mut out = Vec::new();
    for (row, record) in reader.records().enumerate() {
        let record =
            record.map_err(|e| format!("reading {} row {}: {e}", path.display(), row + 1))?;
        let cell = |i: usize| record.get(i).unwrap_or_default().trim().to_string();
        let id = |i: usize| -> Result<i64, String> {
            cell(i).parse::<i64>().map_err(|_| {
                format!(
                    "{} row {}: {} '{}' is not an integer",
                    path.display(),
                    row + 1,
                    expected[i],
                    cell(i)
                )
            })
        };
        for i in [1usize, 3] {
            if cell(i).is_empty() {
                return Err(format!(
                    "{} row {}: {} is empty",
                    path.display(),
                    row + 1,
                    expected[i]
                ));
            }
        }
        out.push((id(0)?, cell(1), id(2)?, cell(3)));
    }
    if out.is_empty() {
        return Err(format!("{} has a header but no rows", path.display()));
    }
    Ok(out)
}

fn require_id(value: Option<i64>, flag: &str) -> Result<i64, String> {
    value.ok_or_else(|| format!("{flag} is required (or pass --from <path.csv> for a batch)"))
}

fn require_type(value: Option<&str>, flag: &str) -> Result<String, String> {
    value
        .map(str::to_string)
        .ok_or_else(|| format!("{flag} is required (or pass --from <path.csv> for a batch)"))
}
