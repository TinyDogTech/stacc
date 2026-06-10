//! Configuration: autodetection of trunk and remote, with precedence
//! resolution over command-line flags and a config file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use stacc_git::{Git, GitError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse config file: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("failed to edit config file: {0}")]
    Edit(#[from] toml_edit::TomlError),

    #[error("cannot edit `{key}`: `aliases` is not a table in the config file")]
    AliasesNotATable { key: String },

    #[error("could not determine {field}; pass it explicitly")]
    Missing { field: &'static str },
}

/// A key the config files support: the top-level `trunk` and `remote`
/// overrides, and `aliases.<name>` entries in the `[aliases]` table. This is
/// the complete settable namespace; `stacc config` validates against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Trunk,
    Remote,
    Alias(String),
}

impl Key {
    /// Parse a dotted key string. `None` means the key is not one stacc knows.
    pub fn parse(key: &str) -> Option<Key> {
        match key {
            "trunk" => Some(Key::Trunk),
            "remote" => Some(Key::Remote),
            _ => match key.strip_prefix("aliases.") {
                Some(name) if !name.is_empty() && !name.contains('.') => {
                    Some(Key::Alias(name.to_string()))
                }
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Trunk => f.write_str("trunk"),
            Key::Remote => f.write_str("remote"),
            Key::Alias(name) => write!(f, "aliases.{name}"),
        }
    }
}

/// User-supplied values (from flags or a config file). A `None` field falls
/// through to the next source during resolution.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Overrides {
    pub trunk: Option<String>,
    pub remote: Option<String>,
}

/// Values discovered from the repository.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Detected {
    pub trunk: Option<String>,
    pub remote: Option<String>,
}

/// The fully-resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub trunk: String,
    pub remote: String,
}

/// Read a TOML config file. A missing file yields empty overrides.
pub fn read_file(path: &Path) -> Result<Overrides, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Overrides::default()),
        Err(err) => Err(err.into()),
    }
}

/// Read the `[aliases]` table from `path`. Best-effort: a missing file, an
/// absent table, or invalid TOML all yield an empty map (an alias misconfig
/// shouldn't bring down `stacc --version`).
pub fn aliases_from_file(path: &Path) -> BTreeMap<String, String> {
    #[derive(Default, Deserialize)]
    struct Wrap {
        #[serde(default)]
        aliases: BTreeMap<String, String>,
    }

    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    toml::from_str::<Wrap>(&text).unwrap_or_default().aliases
}

/// Set `key` to `value` in the TOML file at `path`, creating the file (and
/// its parent directories) when missing. The edit is format-preserving:
/// unrelated keys, comments, and layout survive the round-trip.
pub fn set_in_file(path: &Path, key: &Key, value: &str) -> Result<(), ConfigError> {
    let mut doc = load_document(path)?;
    match key {
        Key::Trunk => doc["trunk"] = toml_edit::value(value),
        Key::Remote => doc["remote"] = toml_edit::value(value),
        Key::Alias(name) => {
            let aliases = doc
                .entry("aliases")
                .or_insert(toml_edit::table())
                .as_table_mut()
                .ok_or_else(|| ConfigError::AliasesNotATable {
                    key: format!("aliases.{name}"),
                })?;
            aliases[name.as_str()] = toml_edit::value(value);
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Remove `key` from the TOML file at `path`. Idempotent: a missing file or
/// an absent key is a no-op (and never creates the file). Removing the last
/// alias also removes the then-empty `[aliases]` table.
pub fn unset_in_file(path: &Path, key: &Key) -> Result<(), ConfigError> {
    if !path.exists() {
        return Ok(());
    }
    let mut doc = load_document(path)?;
    match key {
        Key::Trunk => {
            doc.remove("trunk");
        }
        Key::Remote => {
            doc.remove("remote");
        }
        Key::Alias(name) => {
            if let Some(aliases) = doc.get_mut("aliases").and_then(toml_edit::Item::as_table_mut)
            {
                aliases.remove(name);
                if aliases.is_empty() {
                    doc.remove("aliases");
                }
            }
        }
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Parse `path` as an editable TOML document; a missing file is an empty one.
fn load_document(path: &Path) -> Result<toml_edit::DocumentMut, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    Ok(text.parse()?)
}

/// The user-global stacc config path, conventionally at
/// `~/.config/stacc/config.toml`.
pub fn user_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/stacc/config.toml")
}

/// Detect the trunk and remote from the repository.
pub fn detect(git: &Git) -> Result<Detected, ConfigError> {
    let remotes = git.remotes()?;
    let remote = if remotes.iter().any(|r| r == "origin") {
        Some("origin".to_string())
    } else {
        remotes.into_iter().next()
    };
    let trunk = detect_trunk(git, remote.as_deref())?;
    Ok(Detected { trunk, remote })
}

fn detect_trunk(git: &Git, remote: Option<&str>) -> Result<Option<String>, ConfigError> {
    if let Some(remote) = remote {
        let head = format!("refs/remotes/{remote}/HEAD");
        if let Some(target) = git.symbolic_ref(&head)? {
            let branch = target
                .strip_prefix(&format!("{remote}/"))
                .unwrap_or(target.as_str());
            return Ok(Some(branch.to_string()));
        }
    }
    for candidate in ["main", "master"] {
        if git.ref_commit(&format!("refs/heads/{candidate}"))?.is_some() {
            return Ok(Some(candidate.to_string()));
        }
    }
    Ok(None)
}

/// Resolve the final config. Precedence: `flags` > `file` > `detected`.
pub fn resolve(
    detected: Detected,
    file: Overrides,
    flags: Overrides,
) -> Result<Config, ConfigError> {
    let trunk = flags
        .trunk
        .or(file.trunk)
        .or(detected.trunk)
        .ok_or(ConfigError::Missing { field: "trunk" })?;
    let remote = flags
        .remote
        .or(file.remote)
        .or(detected.remote)
        .ok_or(ConfigError::Missing { field: "remote" })?;
    Ok(Config { trunk, remote })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> (TempDir, Git) {
        let tmp = TempDir::new().expect("temp dir");
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.name", "Test"]);
        run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
        run_git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        let git = Git::open(tmp.path());
        (tmp, git)
    }

    #[test]
    fn detects_trunk_and_origin() {
        let (tmp, git) = init_repo();
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://example.com/r.git"],
        );
        let detected = detect(&git).unwrap();
        assert_eq!(detected.trunk.as_deref(), Some("main"));
        assert_eq!(detected.remote.as_deref(), Some("origin"));
    }

    #[test]
    fn detect_without_remote_still_finds_trunk() {
        let (_tmp, git) = init_repo();
        let detected = detect(&git).unwrap();
        assert_eq!(detected.remote, None);
        assert_eq!(detected.trunk.as_deref(), Some("main"));
    }

    #[test]
    fn resolve_precedence_flags_over_file_over_detected() {
        let detected = Detected {
            trunk: Some("main".into()),
            remote: Some("origin".into()),
        };
        let file = Overrides {
            trunk: Some("develop".into()),
            remote: None,
        };
        let flags = Overrides {
            trunk: None,
            remote: Some("upstream".into()),
        };
        let cfg = resolve(detected, file, flags).unwrap();
        assert_eq!(cfg.trunk, "develop"); // file beats detected
        assert_eq!(cfg.remote, "upstream"); // flag beats detected
    }

    #[test]
    fn resolve_errors_when_field_missing() {
        let err = resolve(
            Detected::default(),
            Overrides::default(),
            Overrides::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Missing { field: "trunk" }));
    }

    #[test]
    fn read_file_parses_toml_and_tolerates_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stacc.toml");
        assert_eq!(read_file(&path).unwrap(), Overrides::default());

        std::fs::write(&path, "trunk = \"main\"\nremote = \"origin\"\n").unwrap();
        let overrides = read_file(&path).unwrap();
        assert_eq!(overrides.trunk.as_deref(), Some("main"));
        assert_eq!(overrides.remote.as_deref(), Some("origin"));
    }

    #[test]
    fn key_parse_accepts_the_full_namespace_and_nothing_else() {
        assert_eq!(Key::parse("trunk"), Some(Key::Trunk));
        assert_eq!(Key::parse("remote"), Some(Key::Remote));
        assert_eq!(Key::parse("aliases.co"), Some(Key::Alias("co".into())));
        assert_eq!(Key::parse("aliases."), None);
        assert_eq!(Key::parse("aliases.a.b"), None);
        assert_eq!(Key::parse("aliases"), None);
        assert_eq!(Key::parse("bogus"), None);
        assert_eq!(Key::Alias("co".into()).to_string(), "aliases.co");
    }

    #[test]
    fn set_in_file_creates_the_file_and_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/dir/config.toml");
        set_in_file(&path, &Key::Trunk, "main").unwrap();
        let overrides = read_file(&path).unwrap();
        assert_eq!(overrides.trunk.as_deref(), Some("main"));
    }

    #[test]
    fn set_in_file_preserves_unrelated_keys_and_comments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# my settings\nremote = \"origin\" # keep me\n\n[aliases]\nco = \"checkout\"\n",
        )
        .unwrap();

        set_in_file(&path, &Key::Trunk, "develop").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my settings"), "got: {text}");
        assert!(text.contains("# keep me"), "got: {text}");
        assert!(text.contains("co = \"checkout\""), "got: {text}");

        let overrides = read_file(&path).unwrap();
        assert_eq!(overrides.trunk.as_deref(), Some("develop"));
        assert_eq!(overrides.remote.as_deref(), Some("origin"));
    }

    #[test]
    fn set_in_file_overwrites_an_existing_value() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_in_file(&path, &Key::Remote, "origin").unwrap();
        set_in_file(&path, &Key::Remote, "upstream").unwrap();
        assert_eq!(read_file(&path).unwrap().remote.as_deref(), Some("upstream"));
    }

    #[test]
    fn set_alias_creates_the_table_and_the_loader_reads_it_back() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_in_file(&path, &Key::Alias("co".into()), "checkout").unwrap();
        set_in_file(&path, &Key::Alias("ll".into()), "log long").unwrap();

        let aliases = aliases_from_file(&path);
        assert_eq!(aliases.get("co").map(String::as_str), Some("checkout"));
        assert_eq!(aliases.get("ll").map(String::as_str), Some("log long"));
    }

    #[test]
    fn set_alias_on_a_non_table_aliases_key_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "aliases = \"oops\"\n").unwrap();
        let err = set_in_file(&path, &Key::Alias("co".into()), "checkout").unwrap_err();
        assert!(matches!(err, ConfigError::AliasesNotATable { .. }));
    }

    #[test]
    fn unset_in_file_removes_the_key_and_keeps_the_rest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // A comment attached to a surviving key stays; one attached to the
        // removed key leaves with it (toml_edit ties comments to their key).
        std::fs::write(
            &path,
            "trunk = \"main\"\n# keep\nremote = \"origin\" # inline\n",
        )
        .unwrap();

        unset_in_file(&path, &Key::Trunk).unwrap();
        let overrides = read_file(&path).unwrap();
        assert_eq!(overrides.trunk, None);
        assert_eq!(overrides.remote.as_deref(), Some("origin"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep"), "got: {text}");
        assert!(text.contains("# inline"), "got: {text}");
    }

    #[test]
    fn unset_last_alias_removes_the_empty_table() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_in_file(&path, &Key::Alias("co".into()), "checkout").unwrap();
        unset_in_file(&path, &Key::Alias("co".into())).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[aliases]"), "got: {text}");
        assert!(aliases_from_file(&path).is_empty());
    }

    #[test]
    fn unset_in_file_is_a_no_op_on_a_missing_file_or_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        unset_in_file(&path, &Key::Trunk).unwrap();
        assert!(!path.exists(), "unset must not create the file");

        std::fs::write(&path, "remote = \"origin\"\n").unwrap();
        unset_in_file(&path, &Key::Alias("co".into())).unwrap();
        assert_eq!(read_file(&path).unwrap().remote.as_deref(), Some("origin"));
    }
}
