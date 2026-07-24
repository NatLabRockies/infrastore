//! Tiny ANSI color helpers (green bold for
//! headers/success, cyan for literals, dim for secondary notes).
//!
//! Color is emitted only when stdout is a terminal and `NO_COLOR` is unset, so
//! piped/redirected output (and `-f json`/`-f csv`, which is consumed by other
//! tools) stays plain.

use std::io::IsTerminal;
use std::sync::OnceLock;

const GREEN_BOLD: &str = "\x1b[1;32m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Whether color should be emitted to stdout (cached for the process).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

/// Section/table headers and success messages: green + bold.
pub fn header(s: &str) -> String {
    paint(GREEN_BOLD, s)
}

/// Literals / field labels: cyan.
pub fn label(s: &str) -> String {
    paint(CYAN, s)
}

/// Secondary text: dim (truncation notes, "no results").
pub fn dim(s: &str) -> String {
    paint(DIM, s)
}
