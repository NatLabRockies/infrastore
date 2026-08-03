//! Interactive confirmation, and the global `--yes` that turns it off.
//!
//! Every destructive command used to carry its own `--force`, which meant a
//! script had to know which commands prompt and spell the right flag for each.
//! A single global `--yes` answers all of them; the per-command `--force` flags
//! stay, because they are what a one-off invocation reaches for and they read
//! better next to the command they belong to.

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

static ASSUME_YES: OnceLock<bool> = OnceLock::new();

/// Record the global `--yes`. Called once, from `main`.
pub fn set_assume_yes(yes: bool) {
    let _ = ASSUME_YES.set(yes);
}

fn assume_yes() -> bool {
    *ASSUME_YES.get().unwrap_or(&false)
}

/// Ask before doing something destructive but recoverable.
///
/// A non-interactive run proceeds: there is nobody to answer, and every caller
/// of this is an operation the invocation explicitly named (`remove`, `clear`,
/// `compact`), so refusing would break every script that already works.
pub fn ask(prompt: &str) -> Result<bool, String> {
    if assume_yes() || !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    prompt_yes_no(prompt)
}

/// Ask before doing something whose failure destroys data the command was not
/// asked to touch.
///
/// Unlike [`ask`], a non-interactive run *stops* rather than proceeding: the
/// caller has to say `--force` (or the global `--yes`) out loud. `persist` over
/// an existing artifact is the case this exists for — a save that fails partway
/// can leave neither the old nor the new pair on disk.
pub fn ask_strict(prompt: &str, how_to_force: &str) -> Result<bool, String> {
    if assume_yes() {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{prompt}\nRefusing without confirmation: {how_to_force}"
        ));
    }
    prompt_yes_no(prompt)
}

/// Prompt on stderr, not stdout.
///
/// The question and the abort notice are addressed to whoever is sitting at the
/// terminal, not to whatever is reading the command's output — and a prompt
/// written to stdout lands in the middle of the JSON that `-f json | jq` is
/// waiting on. stderr is still the terminal in the interactive case this only
/// runs in, so the human sees it either way.
fn prompt_yes_no(prompt: &str) -> Result<bool, String> {
    let mut err = std::io::stderr();
    write!(err, "{prompt}").ok();
    err.flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(true)
    } else {
        writeln!(err, "{}", crate::color::dim_err("Aborted.")).ok();
        Ok(false)
    }
}
