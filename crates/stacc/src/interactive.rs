//! TTY-gated interactive selection. Every prompt is reached only after
//! [`allowed`] confirms a real terminal, no `--no-interactive`, and pretty
//! output, so no agent/non-interactive path can ever block on input.

use std::io::Write;

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
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{prompt}");
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(err, "  {}) {item}", i + 1);
    }
    let _ = write!(err, "Select [1-{}]: ", items.len());
    let _ = err.flush();

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
