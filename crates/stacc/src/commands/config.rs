//! `stacc config`: a non-interactive get/set surface over the config files
//! stacc already reads (KTD-6): the repo-local `.stacc.toml` and the
//! user-global config. File-level only: it never touches the state ref, so
//! every subcommand works outside an initialized (or even git) repository.
//!
//! Resolution mirrors how the values are consumed: repo file over global file,
//! falling back to repository detection for `trunk`/`remote` and to the
//! shipped defaults for aliases. An interactive TTY menu is a possible future
//! convenience; the non-interactive surface is the contract.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;
use stacc_config::{aliases_from_file, detect, read_file, set_in_file, unset_in_file, user_config_path, Detected, Key};
use stacc_git::Git;

use crate::cli::{ConfigAction, ConfigArgs, OutputFormat};
use crate::error::Error;

/// The repo-local config file, resolved relative to the working directory
/// (matching `init` and the alias loader).
const REPO_FILE: &str = ".stacc.toml";

/// `stacc config`: dispatch to get / set / unset / list.
pub fn config(args: &ConfigArgs, format: OutputFormat) -> Result<(), Error> {
    match &args.action {
        ConfigAction::Get { key } => get(key, format),
        ConfigAction::Set { key, value, global } => set(key, value, *global, format),
        ConfigAction::Unset { key, global } => unset(key, *global, format),
        ConfigAction::List => list(format),
    }
}

/// Parse a key string, or fail with the complete valid namespace named.
fn parse_key(key: &str) -> Result<Key, Error> {
    Key::parse(key).ok_or_else(|| {
        Error::Usage(format!(
            "unknown config key `{key}`; valid keys: trunk, remote, aliases.<name>"
        ))
    })
}

/// The file a write targets: the user-global config with `--global`, the
/// repo-local `.stacc.toml` otherwise.
fn target_file(global: bool) -> PathBuf {
    if global {
        user_config_path()
    } else {
        PathBuf::from(REPO_FILE)
    }
}

/// A resolved key: its effective value and where it came from.
struct Resolved {
    value: Option<String>,
    source: Option<&'static str>,
}

impl Resolved {
    fn new(value: String, source: &'static str) -> Self {
        Resolved {
            value: Some(value),
            source: Some(source),
        }
    }

    const UNSET: Resolved = Resolved {
        value: None,
        source: None,
    };

    fn as_json(&self) -> serde_json::Value {
        json!({ "value": self.value, "source": self.source })
    }
}

/// Everything the config files (plus detection and shipped defaults) say,
/// pre-loaded once so get and list resolve identically.
struct Sources {
    repo: stacc_config::Overrides,
    global: stacc_config::Overrides,
    detected: Detected,
    repo_aliases: BTreeMap<String, String>,
    global_aliases: BTreeMap<String, String>,
    default_aliases: BTreeMap<String, String>,
}

impl Sources {
    /// Load both files (malformed TOML is a real error) and detect
    /// best-effort: outside a git repository detection just yields nothing.
    fn load() -> Result<Sources, Error> {
        let global_path = user_config_path();
        Ok(Sources {
            repo: read_file(Path::new(REPO_FILE))?,
            global: read_file(&global_path)?,
            detected: detect(&Git::open(".")).unwrap_or_default(),
            repo_aliases: aliases_from_file(Path::new(REPO_FILE)),
            global_aliases: aliases_from_file(&global_path),
            default_aliases: crate::DEFAULT_ALIASES
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        })
    }

    /// Resolve one key. Precedence: repo > global > detected (trunk/remote)
    /// or the shipped default (aliases).
    fn resolve(&self, key: &Key) -> Resolved {
        match key {
            Key::Trunk => Self::layered(
                self.repo.trunk.as_ref(),
                self.global.trunk.as_ref(),
                self.detected.trunk.as_ref(),
                "detected",
            ),
            Key::Remote => Self::layered(
                self.repo.remote.as_ref(),
                self.global.remote.as_ref(),
                self.detected.remote.as_ref(),
                "detected",
            ),
            Key::Alias(name) => Self::layered(
                self.repo_aliases.get(name),
                self.global_aliases.get(name),
                self.default_aliases.get(name),
                "default",
            ),
        }
    }

    fn layered(
        repo: Option<&String>,
        global: Option<&String>,
        fallback: Option<&String>,
        fallback_label: &'static str,
    ) -> Resolved {
        if let Some(value) = repo {
            Resolved::new(value.clone(), "repo")
        } else if let Some(value) = global {
            Resolved::new(value.clone(), "global")
        } else if let Some(value) = fallback {
            Resolved::new(value.clone(), fallback_label)
        } else {
            Resolved::UNSET
        }
    }

    /// Every alias name any source knows, for `list`.
    fn alias_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .default_aliases
            .keys()
            .chain(self.global_aliases.keys())
            .chain(self.repo_aliases.keys())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

fn get(key: &str, format: OutputFormat) -> Result<(), Error> {
    let key = parse_key(key)?;
    let resolved = Sources::load()?.resolve(&key);

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "config",
                "key": key.to_string(),
                "value": resolved.value,
                "source": resolved.source,
            })
        ),
        OutputFormat::Pretty => match &resolved.value {
            Some(value) => println!("{value}"),
            None => println!("unset"),
        },
    }
    Ok(())
}

fn set(key: &str, value: &str, global: bool, format: OutputFormat) -> Result<(), Error> {
    let key = parse_key(key)?;
    let file = target_file(global);
    set_in_file(&file, &key, value)?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "config",
                "status": "set",
                "key": key.to_string(),
                "value": value,
                "file": file.display().to_string(),
            })
        ),
        OutputFormat::Pretty => println!("Set {key} = {value} in {}", file.display()),
    }
    Ok(())
}

fn unset(key: &str, global: bool, format: OutputFormat) -> Result<(), Error> {
    let key = parse_key(key)?;
    let file = target_file(global);
    unset_in_file(&file, &key)?;

    match format {
        OutputFormat::Json => println!(
            "{}",
            json!({
                "op": "config",
                "status": "unset",
                "key": key.to_string(),
                "file": file.display().to_string(),
            })
        ),
        OutputFormat::Pretty => println!("Unset {key} in {}", file.display()),
    }
    Ok(())
}

fn list(format: OutputFormat) -> Result<(), Error> {
    let sources = Sources::load()?;

    // (key, resolved) for every known key: trunk, remote, then each alias.
    let mut rows: Vec<(String, Resolved)> = vec![
        ("trunk".to_string(), sources.resolve(&Key::Trunk)),
        ("remote".to_string(), sources.resolve(&Key::Remote)),
    ];
    let aliases: Vec<(String, Resolved)> = sources
        .alias_names()
        .into_iter()
        .map(|name| {
            let resolved = sources.resolve(&Key::Alias(name.clone()));
            (name, resolved)
        })
        .collect();

    match format {
        OutputFormat::Json => {
            let values: serde_json::Map<String, serde_json::Value> = rows
                .iter()
                .map(|(key, resolved)| (key.clone(), resolved.as_json()))
                .collect();
            let alias_values: serde_json::Map<String, serde_json::Value> = aliases
                .iter()
                .map(|(name, resolved)| (name.clone(), resolved.as_json()))
                .collect();
            println!(
                "{}",
                json!({ "op": "config", "values": values, "aliases": alias_values })
            );
        }
        OutputFormat::Pretty => {
            rows.extend(
                aliases
                    .into_iter()
                    .map(|(name, resolved)| (format!("aliases.{name}"), resolved)),
            );
            let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            let value_width = rows
                .iter()
                .map(|(_, r)| r.value.as_deref().unwrap_or("(unset)").len())
                .max()
                .unwrap_or(0);
            for (key, resolved) in &rows {
                let value = resolved.value.as_deref().unwrap_or("(unset)");
                let source = resolved.source.unwrap_or("-");
                println!("{key:<key_width$}  {value:<value_width$}  {source}");
            }
        }
    }
    Ok(())
}
