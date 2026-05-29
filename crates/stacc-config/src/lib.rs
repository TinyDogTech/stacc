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

    #[error("could not determine {field}; pass it explicitly")]
    Missing { field: &'static str },
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
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };

    #[derive(Default, Deserialize)]
    struct Wrap {
        #[serde(default)]
        aliases: BTreeMap<String, String>,
    }

    toml::from_str::<Wrap>(&text).unwrap_or_default().aliases
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
}
