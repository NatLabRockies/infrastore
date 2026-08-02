//! Tiny ANSI color helpers (green bold for
//! headers/success, cyan for literals, dim for secondary notes).
//!
//! Color is emitted only when the destination stream is a terminal and
//! `NO_COLOR` is unset, so piped/redirected output (and `-f json`/`-f csv`,
//! which is consumed by other tools) stays plain. The two streams are tracked
//! separately: `infrastore ... | jq` leaves stderr a terminal even though stdout
//! is not, and the prompts and notices that go there should still be colored.

use std::io::IsTerminal;
use std::sync::OnceLock;

const GREEN_BOLD: &str = "\x1b[1;32m";
const CYAN: &str = "\x1b[36m";
const CYAN_BOLD: &str = "\x1b[1;36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Whether color should be emitted to stdout (cached for the process).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// Whether color should be emitted to stderr (cached for the process).
fn enabled_err() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal())
}

fn paint(code: &str, s: &str) -> String {
    paint_if(enabled(), code, s)
}

fn paint_if(on: bool, code: &str, s: &str) -> String {
    if on {
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

/// [`dim`], for text bound for stderr rather than stdout.
pub fn dim_err(s: &str) -> String {
    paint_if(enabled_err(), DIM, s)
}

/// Command names in the grouped `--help` listing: cyan + bold, matching the
/// `literal` style clap applies to the flags it renders in the same output.
pub fn literal(s: &str) -> String {
    paint(CYAN_BOLD, s)
}
