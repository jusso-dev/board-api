use serde::Deserialize;
use std::{
    fs,
    net::{IpAddr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG_PATH: &str = "/home/board/.config/board-api/config.json";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub listen: IpAddr,
    pub port: u16,
    pub state_dir: PathBuf,
    pub work_dir: PathBuf,
    pub allowed_harnesses: Vec<String>,
    #[serde(default)]
    pub allowed_issue_authors: Vec<String>,
    #[serde(default)]
    pub auto_run: AutoRunConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutoRunConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default = "default_harness")]
    pub default_harness: String,
}

impl Default for AutoRunConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_seconds: default_poll_seconds(),
            default_harness: default_harness(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("BOARD_API_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("cannot stat config {}: {error}", path.display()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "config {} must not be accessible by group or others (mode {mode:o})",
                path.display()
            ));
        }

        let bytes = fs::read(path)
            .map_err(|error| format!("cannot read config {}: {error}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid config {}: {error}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port must be greater than zero".into());
        }
        if !self.state_dir.is_absolute() || !self.work_dir.is_absolute() {
            return Err("stateDir and workDir must be absolute paths".into());
        }
        if self.allowed_harnesses.is_empty() {
            return Err("allowedHarnesses must not be empty".into());
        }
        for harness in &self.allowed_harnesses {
            if !matches!(harness.as_str(), "grok" | "codex" | "cursor") {
                return Err(format!("unsupported allowed harness: {harness}"));
            }
        }
        if self.allowed_issue_authors.is_empty() {
            return Err("allowedIssueAuthors must not be empty".into());
        }
        for author in &self.allowed_issue_authors {
            if author.is_empty() || author.trim() != author {
                return Err("allowedIssueAuthors entries must be non-empty GitHub logins".into());
            }
        }
        if !(30..=3_600).contains(&self.auto_run.poll_seconds) {
            return Err("autoRun.pollSeconds must be between 30 and 3600".into());
        }
        if !matches!(
            self.auto_run.default_harness.as_str(),
            "grok" | "codex" | "cursor"
        ) {
            return Err(format!(
                "unsupported autoRun.defaultHarness: {}",
                self.auto_run.default_harness
            ));
        }
        if self.auto_run.enabled
            && !self
                .allowed_harnesses
                .contains(&self.auto_run.default_harness)
        {
            return Err("autoRun.defaultHarness must be in allowedHarnesses".into());
        }
        Ok(())
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.listen, self.port)
    }

    pub fn allows_issue_author(&self, author: Option<&str>) -> bool {
        author.is_some_and(|author| {
            self.allowed_issue_authors
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(author))
        })
    }
}

fn default_poll_seconds() -> u64 {
    60
}

fn default_harness() -> String {
    "codex".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_harness() {
        let config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            allowed_harnesses: vec!["unknown".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            auto_run: AutoRunConfig::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_enabled_default_harness_that_is_not_allowed() {
        let config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            allowed_harnesses: vec!["grok".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            auto_run: AutoRunConfig {
                enabled: true,
                poll_seconds: 60,
                default_harness: "codex".into(),
            },
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn issue_author_allowlist_is_case_insensitive_and_fails_closed() {
        let config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            allowed_harnesses: vec!["codex".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            auto_run: AutoRunConfig::default(),
        };

        assert!(config.allows_issue_author(Some("TRUSTED-USER")));
        assert!(!config.allows_issue_author(Some("someone-else")));
        assert!(!config.allows_issue_author(None));

        let mut missing = config;
        missing.allowed_issue_authors.clear();
        assert!(missing.validate().is_err());
    }
}
