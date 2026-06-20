//! TTY-gated interactive selection. Prompts are reached only after [`allowed`]
//! confirms a real terminal, no `--no-interactive`, and pretty output.

use std::io::Write;

use inquire::InquireError;

use crate::cli::OutputFormat;
use crate::error::Error;

/// Whether an interactive prompt is permitted: a real terminal, not in
/// `--no-interactive` mode, and not emitting machine JSON.
pub fn allowed(is_terminal: bool, no_interactive: bool, format: OutputFormat) -> bool {
    is_terminal && !no_interactive && !matches!(format, OutputFormat::Json)
}

/// Render a numbered menu to stderr and read a 1-based choice from stdin. Only
/// call once [`allowed`] has gated the path.
pub fn prompt_select(prompt: &str, items: &[String]) -> Result<String, Error> {
    eprintln!("{prompt}");
    for (i, item) in items.iter().enumerate() {
        eprintln!("  {}) {item}", i + 1);
    }
    eprint!("Select [1-{}]: ", items.len());
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Error::Usage(format!("could not read selection: {e}")))?;
    let choice: usize = line
        .trim()
        .parse()
        .ok()
        .filter(|n| (1..=items.len()).contains(n))
        .ok_or_else(|| Error::Usage(format!("invalid selection `{}`", line.trim())))?;
    Ok(items[choice - 1].clone())
}

/// Present a checkbox-style multi-select using `inquire`. Returns the indices of
/// the items the user selected. Pressing Esc or selecting none returns an empty
/// `Vec` rather than an error -- callers treat that as "decline all".
///
/// Panics if `items` is empty; callers must guard with a non-empty check first.
pub fn prompt_multi_select(prompt: &str, items: &[String]) -> Result<Vec<usize>, Error> {
    let result = inquire::MultiSelect::new(prompt, items.to_vec()).prompt();
    match result {
        Ok(selected) => {
            let indices = selected
                .iter()
                .filter_map(|s| items.iter().position(|i| i == s))
                .collect();
            Ok(indices)
        }
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(vec![]),
        Err(e) => Err(Error::Usage(e.to_string())),
    }
}

/// Prompt the user to confirm or override a pre-filled value. Returns the
/// accepted or overridden string. Pressing Esc keeps the default.
pub fn prompt_confirm_or_change(prompt: &str, default: &str) -> Result<String, Error> {
    let result = inquire::Text::new(prompt).with_default(default).prompt();
    match result {
        Ok(val) => Ok(val),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            Ok(default.to_owned())
        }
        Err(e) => Err(Error::Usage(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_only_on_a_pretty_interactive_terminal() {
        assert!(allowed(true, false, OutputFormat::Pretty));
        assert!(!allowed(false, false, OutputFormat::Pretty)); // not a tty
        assert!(!allowed(true, true, OutputFormat::Pretty)); // --no-interactive
        assert!(!allowed(true, false, OutputFormat::Json)); // machine output
        assert!(!allowed(false, true, OutputFormat::Json)); // none of the above
    }
}
