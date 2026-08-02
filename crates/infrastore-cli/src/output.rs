use std::io::Write;

use serde::Serialize;
use tabled::builder::Builder;
use tabled::settings::Style;

/// Output rendering selected by `-f/--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    /// Line-delimited JSON: one compact object per line, no enclosing array.
    ///
    /// `json` emits one pretty document, which a 100k-row `list` cannot be
    /// streamed out of — `jq` has to buffer the whole array before it sees the
    /// first element. `jsonl` is the same data in the shape every streaming
    /// consumer expects.
    Jsonl,
    Csv,
}

impl Format {
    /// Whether this format is rendered by the JSON writers rather than the
    /// table/CSV ones.
    pub fn is_json(self) -> bool {
        matches!(self, Format::Json | Format::Jsonl)
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Table => "table",
            Format::Json => "json",
            Format::Jsonl => "jsonl",
            Format::Csv => "csv",
        })
    }
}

/// Render a header + rows as a rounded console table. Header cells are colored
/// green + bold when stdout is a color-capable terminal.
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

/// Print a list of values wrapped as `{"items": [...]}`.
pub fn print_json_wrapped<T: Serialize>(items: &[T]) -> Result<(), String> {
    let wrapped = serde_json::json!({ "items": items });
    print_json(&wrapped)
}

/// Print one value in whichever JSON shape `format` asks for.
///
/// A single document has no "elements" to stream, so `jsonl` renders it as one
/// compact line rather than as a one-element stream — which is what a
/// line-oriented consumer of a single record wants anyway.
pub fn print_value<T: Serialize>(format: Format, value: &T) -> Result<(), String> {
    match format {
        Format::Jsonl => {
            let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
            write_line(&text)
        }
        _ => print_json(value),
    }
}

/// Print a list of values: one `{"items": [...]}` document, or one compact
/// object per line.
pub fn print_items<T: Serialize>(format: Format, items: &[T]) -> Result<(), String> {
    match format {
        Format::Jsonl => {
            for item in items {
                let text = serde_json::to_string(item).map_err(|e| e.to_string())?;
                write_line(&text)?;
            }
            Ok(())
        }
        _ => print_json_wrapped(items),
    }
}

/// Emit the outcome of a command that changed something.
///
/// Under `-f json`/`-f jsonl`, `value` is the command's *only* stdout output, so
/// a scripted mutation pipes into `jq` exactly the way a query already does.
/// Every other format gets whatever `render` prints.
///
/// `csv` deliberately renders as prose alongside `table` rather than as a
/// one-row table: a status line has no rows to tabulate, and inventing a header
/// for it would give scripts a shape that changes with every message we reword.
/// JSON is the machine-readable channel here; CSV is not.
pub fn report(
    format: Format,
    value: serde_json::Value,
    render: impl FnOnce(),
) -> Result<(), String> {
    match format {
        f if f.is_json() => print_value(f, &value),
        _ => {
            render();
            Ok(())
        }
    }
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
