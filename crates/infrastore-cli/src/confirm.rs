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
    let stdin = std::io::stdin();
    prompt_yes_no_on(&mut stdin.lock(), &mut std::io::stderr(), prompt)
}

/// The body of [`prompt_yes_no`], over any reader and writer.
///
/// Split out so the answers can be tested: the real one only runs against a
/// terminal, and a test has none. What it decides is small but not nothing --
/// which spellings mean yes, that the comparison ignores case and surrounding
/// space, and that everything else (including EOF) is a no.
fn prompt_yes_no_on<R: std::io::BufRead, W: Write>(
    input: &mut R,
    err: &mut W,
    prompt: &str,
) -> Result<bool, String> {
    write!(err, "{prompt}").ok();
    err.flush().ok();
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(|e| e.to_string())?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(true)
    } else {
        writeln!(err, "{}", crate::color::dim_err("Aborted.")).ok();
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(text: &str) -> (bool, String) {
        let mut err = Vec::new();
        let yes = prompt_yes_no_on(&mut text.as_bytes(), &mut err, "Go? [y/N] ")
            .expect("reading from a slice cannot fail");
        (yes, String::from_utf8(err).expect("utf8"))
    }

    #[test]
    fn only_y_and_yes_mean_yes_whatever_their_case_or_spacing() {
        for text in ["y\n", "yes\n", "Y\n", "YES\n", "  yEs  \n", "y"] {
            assert!(answer(text).0, "{text:?} should confirm");
        }
    }

    #[test]
    fn everything_else_is_a_no_and_says_so() {
        // An empty line is the `[y/N] ` default; EOF is a closed pipe; the rest
        // are near-misses that must not be read as consent.
        for text in ["\n", "", "n\n", "no\n", "yep\n", "ye\n", "sure\n", "1\n"] {
            let (yes, err) = answer(text);
            assert!(!yes, "{text:?} should not confirm");
            assert!(err.contains("Aborted."), "{text:?} should say it stopped");
        }
    }

    #[test]
    fn the_prompt_goes_to_the_error_stream_so_it_cannot_land_in_piped_output() {
        // `-f json | jq` reads stdout; a question written there would corrupt
        // the document it is waiting on.
        let (_, err) = answer("y\n");
        assert!(err.starts_with("Go? [y/N] "), "{err:?}");
    }
}
