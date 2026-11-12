//! Group configuration: which hosts make up a group (S5).
//!
//! The file never carries credentials or connection settings; it only names
//! hosts that the system OpenSSH client already knows how to reach.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_CONCURRENCY: usize = 16;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Connection knobs that dsh forwards to the system `ssh` client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub concurrency: usize,
    pub connect_timeout_secs: u64,
    /// Extra `-o key=value` options appended to every ssh invocation.
    pub ssh_options: Vec<String>,
    /// ssh executable to use (tests point this at a stub).
    pub ssh_bin: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            ssh_options: Vec::new(),
            ssh_bin: "ssh".to_string(),
        }
    }
}

/// `~/.config/dsh/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    /// Group name -> ordered host list. An entry starting with `@` references
    /// another group, so clusters can be composed.
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
}

/// What the user asked to target on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpec {
    Groups(Vec<String>),
    Hosts(Vec<String>),
    All,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no config file found; looked for {0} (create it with a [groups] table, or pass --hosts)"
    )]
    NotFound(String),
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("unknown group `{0}`")]
    UnknownGroup(String),
    #[error("group reference cycle at `{0}`")]
    Cycle(String),
    #[error(
        "invalid host or group entry `{0}`: must not be empty, start with '-' or contain whitespace"
    )]
    InvalidEntry(String),
    #[error("no hosts selected")]
    Empty,
}

impl Config {
    /// Parse a config document without touching the filesystem.
    #[cfg(test)]
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        Self::parse_at(raw, "<memory>")
    }

    fn parse_at(raw: &str, path: &str) -> Result<Self, ConfigError> {
        toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_string(),
            source,
        })
    }

    /// Resolve where the config lives, in precedence order:
    /// `$DSH_CONFIG`, `$XDG_CONFIG_HOME/dsh/config.toml`, `$HOME/.config/dsh/config.toml`.
    pub fn path() -> Result<PathBuf, ConfigError> {
        if let Some(explicit) = std::env::var_os("DSH_CONFIG") {
            return Ok(PathBuf::from(explicit));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("dsh").join("config.toml"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home)
                .join(".config")
                .join("dsh")
                .join("config.toml"));
        }
        Err(ConfigError::NotFound(
            "$HOME/.config/dsh/config.toml".to_string(),
        ))
    }

    pub fn load() -> Result<(Self, PathBuf), ConfigError> {
        let path = Self::path()?;
        Ok((Self::load_from(&path)?, path))
    }

    /// Load a specific file, reporting its path in every error.
    pub fn load_from(path: &std::path::Path) -> Result<Self, ConfigError> {
        let shown = path.display().to_string();
        let raw = std::fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ConfigError::NotFound(shown.clone())
            } else {
                ConfigError::Read {
                    path: shown.clone(),
                    source,
                }
            }
        })?;
        Self::parse_at(&raw, &shown)
    }

    pub fn group_names(&self) -> Vec<String> {
        self.groups.keys().cloned().collect()
    }

    /// Expand a selection into a de-duplicated, order-preserving host list.
    pub fn resolve(&self, spec: &TargetSpec) -> Result<Vec<String>, ConfigError> {
        let mut out: Vec<String> = Vec::new();
        let mut seen_groups: Vec<String> = Vec::new();
        match spec {
            TargetSpec::Groups(groups) => {
                for group in groups {
                    self.expand_group(group, &mut seen_groups, &mut out)?;
                }
            }
            TargetSpec::Hosts(hosts) => {
                for host in hosts {
                    check_entry(host)?;
                    out.push(host.clone());
                }
            }
            TargetSpec::All => {
                for group in self.group_names() {
                    self.expand_group(&group, &mut seen_groups, &mut out)?;
                }
            }
        }

        let mut deduped = Vec::with_capacity(out.len());
        let mut seen = BTreeSet::new();
        for host in out {
            if seen.insert(host.clone()) {
                deduped.push(host);
            }
        }
        if deduped.is_empty() {
            return Err(ConfigError::Empty);
        }
        Ok(deduped)
    }

    fn expand_group(
        &self,
        group: &str,
        seen_groups: &mut Vec<String>,
        out: &mut Vec<String>,
    ) -> Result<(), ConfigError> {
        check_entry(group)?;
        if seen_groups.iter().any(|seen| seen == group) {
            return Err(ConfigError::Cycle(group.to_string()));
        }
        let Some(entries) = self.groups.get(group) else {
            return Err(ConfigError::UnknownGroup(group.to_string()));
        };
        seen_groups.push(group.to_string());
        let result = entries.iter().try_for_each(|entry| {
            check_entry(entry)?;
            match entry.strip_prefix('@') {
                Some(nested) => self.expand_group(nested, seen_groups, out),
                None => {
                    out.push(entry.clone());
                    Ok(())
                }
            }
        });
        seen_groups.pop();
        result
    }
}

fn check_entry(entry: &str) -> Result<(), ConfigError> {
    let invalid =
        entry.is_empty() || entry.starts_with('-') || entry.chars().any(char::is_whitespace);
    if invalid {
        return Err(ConfigError::InvalidEntry(entry.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[defaults]
concurrency = 4
connect_timeout_secs = 2
ssh_options = ["StrictHostKeyChecking=accept-new"]

[groups]
pve = ["pve00.jacd.local", "pve01.jacd.local", "pve02.jacd.local"]
prox = ["prox00.jacd.local"]
cluster = ["@pve", "@prox", "pve00.jacd.local"]
"#;

    fn sample() -> Config {
        Config::parse(SAMPLE).expect("sample config parses")
    }

    #[test]
    fn parses_defaults_and_groups() {
        let config = sample();
        assert_eq!(config.defaults.concurrency, 4);
        assert_eq!(config.defaults.connect_timeout_secs, 2);
        assert_eq!(
            config.defaults.ssh_options,
            vec!["StrictHostKeyChecking=accept-new"]
        );
        assert_eq!(config.group_names(), vec!["cluster", "prox", "pve"]);
    }

    #[test]
    fn missing_defaults_fall_back() {
        let config = Config::parse("[groups]\npve = [\"pve00.jacd.local\"]").expect("parses");
        assert_eq!(config.defaults.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(
            config.defaults.connect_timeout_secs,
            DEFAULT_CONNECT_TIMEOUT_SECS
        );
        assert_eq!(config.defaults.ssh_bin, "ssh");
        assert!(config.defaults.ssh_options.is_empty());
    }

    #[test]
    fn resolves_group_in_order() {
        let hosts = sample()
            .resolve(&TargetSpec::Groups(vec!["pve".into()]))
            .expect("resolves");
        assert_eq!(
            hosts,
            vec!["pve00.jacd.local", "pve01.jacd.local", "pve02.jacd.local"]
        );
    }

    #[test]
    fn nested_groups_expand_and_dedupe() {
        let hosts = sample()
            .resolve(&TargetSpec::Groups(vec!["cluster".into(), "prox".into()]))
            .expect("resolves");
        assert_eq!(
            hosts,
            vec![
                "pve00.jacd.local",
                "pve01.jacd.local",
                "pve02.jacd.local",
                "prox00.jacd.local"
            ]
        );
    }

    #[test]
    fn all_expands_every_group() {
        let hosts = sample().resolve(&TargetSpec::All).expect("resolves");
        assert_eq!(hosts.len(), 4);
    }

    #[test]
    fn explicit_hosts_are_kept_verbatim() {
        let hosts = sample()
            .resolve(&TargetSpec::Hosts(vec![
                "pve00.jacd.local".into(),
                "10.0.0.5".into(),
            ]))
            .expect("resolves");
        assert_eq!(hosts, vec!["pve00.jacd.local", "10.0.0.5"]);
    }

    #[test]
    fn unknown_group_is_reported() {
        let err = sample()
            .resolve(&TargetSpec::Groups(vec!["nope".into()]))
            .expect_err("fails");
        assert_eq!(err.to_string(), "unknown group `nope`");
    }

    #[test]
    fn group_cycles_are_detected() {
        let config = Config::parse("[groups]\na = [\"@b\"]\nb = [\"@a\"]").expect("parses");
        let err = config
            .resolve(&TargetSpec::Groups(vec!["a".into()]))
            .expect_err("fails");
        assert_eq!(err.to_string(), "group reference cycle at `a`");
    }

    #[test]
    fn rejects_unsafe_entries() {
        for entry in ["-oProxyCommand=x", "host name", ""] {
            let err = sample()
                .resolve(&TargetSpec::Hosts(vec![entry.into()]))
                .expect_err("fails");
            assert!(
                err.to_string().starts_with("invalid host or group entry"),
                "{err}"
            );
        }
    }

    #[test]
    fn empty_selection_is_rejected() {
        let config = Config::parse("[groups]").expect("parses");
        let err = config
            .resolve(&TargetSpec::Hosts(vec![]))
            .expect_err("fails");
        assert_eq!(err.to_string(), "no hosts selected");
    }
}
