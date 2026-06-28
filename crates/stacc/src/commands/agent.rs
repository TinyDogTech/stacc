//! `stacc agent`: install agent context files for coding-agent harnesses.

use std::io::IsTerminal;
use std::path::PathBuf;

use serde_json::json;
use stacc_forge::SCHEMA_VERSION;

use crate::cli::{AgentAction, AgentArgs, AgentHarness, AgentInstallArgs, OutputFormat};
use crate::error::Error;
use crate::interactive;

const SKILL_CONTENT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/agent/skill-content.md"));

const SKILL_VERSION: &str = env!("STACC_VERSION");

/// `stacc agent`: dispatch to install.
pub fn agent(args: &AgentArgs, format: OutputFormat, no_interactive: bool) -> Result<(), Error> {
    match &args.action {
        AgentAction::Install(install_args) => agent_install(install_args, format, no_interactive),
    }
}

/// Called from `stacc init` after a fresh initialization to offer the checklist interactively.
/// Any error is non-fatal; callers may warn and continue.
pub(crate) fn agent_install_interactive(format: OutputFormat) -> Result<(), Error> {
    let args = AgentInstallArgs { harness: vec![] };
    agent_install(&args, format, false)
}

fn agent_install(
    args: &AgentInstallArgs,
    format: OutputFormat,
    no_interactive: bool,
) -> Result<(), Error> {
    let targets = resolve_targets(args, format, no_interactive)?;
    if targets.is_empty() {
        if matches!(format, OutputFormat::Json) {
            println!(
                "{}",
                json!({"op":"agent-install","installed":[],"skipped":[],"schema_version":SCHEMA_VERSION})
            );
        }
        return Ok(());
    }

    let home = home_dir()?;
    let mut installed = Vec::new();

    for target in &targets {
        let path = target_path(&home, *target);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Usage(format!("could not create {}: {e}", parent.display()))
            })?;
        }
        let content = target_content(*target);
        std::fs::write(&path, content)
            .map_err(|e| Error::Usage(format!("could not write {}: {e}", path.display())))?;
        installed.push((*target, path));
    }

    // Installing the Claude Code skill supersedes the legacy slash command
    // (`~/.claude/commands/stacc.md`) shipped before STA-129. Remove it so the
    // migration is real on machines that ran the old installer; best-effort,
    // a missing or unremovable file is not an error.
    let mut removed = Vec::new();
    if targets.contains(&AgentHarness::ClaudeSkill) {
        let legacy = home.join(".claude").join("commands").join("stacc.md");
        if legacy.is_file() && std::fs::remove_file(&legacy).is_ok() {
            removed.push(legacy);
        }
    }

    match format {
        OutputFormat::Json => {
            let list: Vec<_> = installed
                .iter()
                .map(|(t, p)| {
                    json!({
                        "target": target_key(*t),
                        "path": p.display().to_string(),
                    })
                })
                .collect();
            let removed_list: Vec<_> = removed.iter().map(|p| p.display().to_string()).collect();
            println!(
                "{}",
                json!({"op":"agent-install","installed":list,"removed":removed_list,"skipped":[],"schema_version":SCHEMA_VERSION})
            );
        }
        OutputFormat::Pretty => {
            for (_, path) in &installed {
                println!("Installed {}", path.display());
            }
            for path in &removed {
                println!("Removed legacy {}", path.display());
            }
        }
    }

    Ok(())
}

fn resolve_targets(
    args: &AgentInstallArgs,
    format: OutputFormat,
    no_interactive: bool,
) -> Result<Vec<AgentHarness>, Error> {
    let explicit: Vec<AgentHarness> = args
        .harness
        .iter()
        .flat_map(|h| match h {
            AgentHarness::All => vec![AgentHarness::Universal, AgentHarness::ClaudeSkill],
            other => vec![*other],
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if !explicit.is_empty() {
        return Ok(explicit);
    }

    if !interactive::allowed(std::io::stdout().is_terminal(), no_interactive, format) {
        return Err(Error::Usage(
            "no harnesses specified; pass --harness or run interactively".into(),
        ));
    }

    let items = vec![
        format!(
            "Universal skill ({}) -- covers all agentskills.io clients",
            target_path_display(AgentHarness::Universal)
        ),
        format!(
            "Claude Code skill ({})",
            target_path_display(AgentHarness::ClaudeSkill)
        ),
    ];

    let indices = interactive::prompt_multi_select("Install agent context files:", &items)?;

    let map = [AgentHarness::Universal, AgentHarness::ClaudeSkill];
    Ok(indices.into_iter().map(|i| map[i]).collect())
}

fn target_path(home: &std::path::Path, target: AgentHarness) -> PathBuf {
    match target {
        AgentHarness::Universal => home.join(".agents").join("skills").join("stacc").join("SKILL.md"),
        AgentHarness::ClaudeSkill => home.join(".claude").join("skills").join("stacc").join("SKILL.md"),
        AgentHarness::All => unreachable!("All is expanded before target_path is called"),
    }
}

fn target_path_display(target: AgentHarness) -> &'static str {
    match target {
        AgentHarness::Universal => "~/.agents/skills/stacc/SKILL.md",
        AgentHarness::ClaudeSkill => "~/.claude/skills/stacc/SKILL.md",
        AgentHarness::All => unreachable!(),
    }
}

fn target_key(target: AgentHarness) -> &'static str {
    match target {
        AgentHarness::Universal => "universal",
        AgentHarness::ClaudeSkill => "claude-skill",
        AgentHarness::All => unreachable!(),
    }
}

/// Both targets are agentskills.io SKILL.md files (frontmatter + canonical body);
/// they differ only by install root, so the content is identical.
fn target_content(target: AgentHarness) -> String {
    match target {
        AgentHarness::Universal | AgentHarness::ClaudeSkill => format!(
            "---\nname: stacc\ndescription: Stacked-diff CLI for AI coding agents -- usage reference\nversion: \"{SKILL_VERSION}\"\n---\n\n{SKILL_CONTENT}"
        ),
        AgentHarness::All => unreachable!(),
    }
}

fn home_dir() -> Result<PathBuf, Error> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| Error::Usage("HOME environment variable is not set".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_content_uses_json_flag() {
        assert!(
            SKILL_CONTENT.contains("--json"),
            "skill content must use --json"
        );
        assert!(
            !SKILL_CONTENT.contains("--format json"),
            "skill content must not use --format json"
        );
    }

    #[test]
    fn universal_content_has_frontmatter() {
        let content = target_content(AgentHarness::Universal);
        assert!(content.starts_with("---\n"), "SKILL.md must start with YAML frontmatter");
        assert!(content.contains("name: stacc"), "SKILL.md must declare name");
    }

    #[test]
    fn claude_skill_content_has_frontmatter() {
        let content = target_content(AgentHarness::ClaudeSkill);
        assert!(content.starts_with("---\n"), "claude skill must start with YAML frontmatter");
        assert!(content.contains("name: stacc"), "claude skill must declare name");
    }

    #[test]
    fn claude_skill_matches_universal_content() {
        // Both targets are the same SKILL.md, only the install root differs.
        assert_eq!(
            target_content(AgentHarness::ClaudeSkill),
            target_content(AgentHarness::Universal),
        );
    }

    #[test]
    fn claude_skill_installs_under_claude_skills_dir() {
        let path = target_path(std::path::Path::new("/home/u"), AgentHarness::ClaudeSkill);
        assert_eq!(path, PathBuf::from("/home/u/.claude/skills/stacc/SKILL.md"));
    }

    #[test]
    fn all_expands_to_two_targets() {
        let args = AgentInstallArgs {
            harness: vec![AgentHarness::All],
        };
        let targets =
            resolve_targets(&args, OutputFormat::Pretty, true).expect("should not need tty");
        assert!(targets.contains(&AgentHarness::Universal));
        assert!(targets.contains(&AgentHarness::ClaudeSkill));
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn no_interactive_no_harness_errors() {
        let args = AgentInstallArgs { harness: vec![] };
        let result = resolve_targets(&args, OutputFormat::Pretty, true);
        assert!(result.is_err());
    }

    #[test]
    fn dedup_repeated_universal() {
        let args = AgentInstallArgs {
            harness: vec![AgentHarness::Universal, AgentHarness::Universal],
        };
        let targets =
            resolve_targets(&args, OutputFormat::Pretty, true).expect("should resolve");
        assert_eq!(targets.len(), 1);
        assert!(targets.contains(&AgentHarness::Universal));
    }
}
