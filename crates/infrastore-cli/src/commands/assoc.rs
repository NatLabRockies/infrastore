//! The `attributes` and `links` commands: read-side views of the two
//! association catalogs.
//!
//! Both catalogs are independent of time series and of each other, so these
//! commands take no time-series selector. They are read-only: writing an
//! association means writing the consumer's object graph too, which is the
//! binding's job, not a CLI one-off's.

use std::path::Path;

use infrastore_core::{ParentChildFilter, SupplementalAttributeFilter};
use serde_json::{Value, json};

use crate::color;
use crate::output::{self, Format};
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
            Format::Json => {
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
                output::print_json_wrapped(&items)
            }
            Format::Csv => output::display_csv_rows(&headers, &table),
            Format::Table => {
                println!("{}", color::header("Supplemental attributes by type"));
                output::display_table_dyn(&headers, &table);
                Ok(())
            }
        };
    }

    let mut filter = SupplementalAttributeFilter::new();
    if let Some(id) = component_id {
        filter = filter.component_id(id);
    }
    if let Some(id) = attribute_id {
        filter = filter.attribute_id(id);
    }
    // The core takes a list of concrete type names; the CLI exposes one, which
    // is the common case and keeps the flag a plain string.
    if let Some(t) = component_type {
        filter = filter.component_types(vec![t.to_string()]);
    }
    if let Some(t) = attribute_type {
        filter = filter.attribute_types(vec![t.to_string()]);
    }

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
        Format::Json => {
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
            output::print_json_wrapped(&items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table)?,
        Format::Table => output::display_table_dyn(&headers, &table),
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
        Format::Json => {
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
            output::print_json_wrapped(&items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table)?,
        Format::Table => output::display_table_dyn(&headers, &table),
    }
    Ok(())
}
