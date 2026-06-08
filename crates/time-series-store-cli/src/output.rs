//! Console output helpers mirroring torc's `output` / `table_format` modules:
//! a `table` (default), `json`, or `csv` rendering selected by the global
//! `-f/--format` flag.

use std::io::Write;

use serde::Serialize;
use tabled::builder::Builder;
use tabled::settings::Style;

/// Output rendering selected by `-f/--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    Csv,
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Table => "table",
            Format::Json => "json",
            Format::Csv => "csv",
        })
    }
}

/// Render a header + rows as a rounded console table. Header cells are colored
/// green + bold (matching `../torc`) when stdout is a color-capable terminal.
pub fn display_table_dyn(headers: &[String], rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("{}", crate::color::dim("(no results)"));
        return;
    }
    let mut builder = Builder::default();
    builder.push_record(headers.iter().map(|h| crate::color::header(h)));
    for row in rows {
        builder.push_record(row.iter().cloned());
    }
    let mut table = builder.build();
    table.with(Style::rounded());
    println!("{table}");
}

/// Render an arbitrary header + rows as CSV, handling a closed pipe gracefully.
pub fn display_csv_rows(headers: &[String], rows: &[Vec<String>]) -> Result<(), String> {
    let mut writer = csv::Writer::from_writer(std::io::stdout());
    if let Err(e) = writer.write_record(headers) {
        return on_csv_err(e);
    }
    for row in rows {
        if let Err(e) = writer.write_record(row) {
            return on_csv_err(e);
        }
    }
    match writer.flush() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => Err(e.to_string()),
    }
}

fn on_csv_err(e: csv::Error) -> Result<(), String> {
    if let csv::ErrorKind::Io(io) = e.kind()
        && io.kind() == std::io::ErrorKind::BrokenPipe
    {
        std::process::exit(0);
    }
    Err(e.to_string())
}

/// Print a single value as pretty JSON.
pub fn print_json<T: Serialize>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_line(&text)
}

/// Print a list of values wrapped as `{"items": [...]}`, matching torc.
pub fn print_json_wrapped<T: Serialize>(items: &[T]) -> Result<(), String> {
    let wrapped = serde_json::json!({ "items": items });
    print_json(&wrapped)
}

/// Write a line to stdout, treating a closed pipe as a clean exit.
fn write_line(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    match writeln!(out, "{text}") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => Err(e.to_string()),
    }
}
