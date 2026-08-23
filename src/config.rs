use crate::model::ReasoningEffort;
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
    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent_jobs: usize,
    pub allowed_harnesses: Vec<String>,
    #[serde(default)]
    pub allowed_issue_authors: Vec<String>,
    #[serde(default)]
    pub cursor_effort_models: CursorEffortModels,
    #[serde(default)]
    pub auto_run: AutoRunConfig,
    #[serde(default)]
    pub cleanup: CleanupConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CursorEffortModels {
    pub low: String,
    pub medium: String,
    pub high: String,
    pub xhigh: String,
}

impl Default for CursorEffortModels {
    fn default() -> Self {
        Self {
            low: "gpt-5.6-sol-low".into(),
            medium: "gpt-5.6-sol-medium".into(),
            high: "gpt-5.6-sol-high".into(),
            xhigh: "gpt-5.6-sol-xhigh".into(),
        }
    }
}

impl CursorEffortModels {
    pub fn model_for(&self, effort: ReasoningEffort) -> &str {
        match effort {
            ReasoningEffort::Low => &self.low,
            ReasoningEffort::Medium => &self.medium,
            ReasoningEffort::High => &self.high,
            ReasoningEffort::Xhigh => &self.xhigh,
        }
    }

    fn values(&self) -> [&str; 4] {
        [&self.low, &self.medium, &self.high, &self.xhigh]
    }
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanupConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cleanup_retention_days")]
    pub retention_days: u16,
    #[serde(default = "default_cleanup_hour_utc")]
    pub run_hour_utc: u8,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: default_cleanup_retention_days(),
            run_hour_utc: default_cleanup_hour_utc(),
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
        if !(1..=16).contains(&self.max_concurrent_jobs) {
            return Err("maxConcurrentJobs must be between 1 and 16".into());
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
        for model in self.cursor_effort_models.values() {
            if model.is_empty() || model.trim() != model {
                return Err("cursorEffortModels entries must be non-empty model names".into());
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
        if !(1..=365).contains(&self.cleanup.retention_days) {
            return Err("cleanup.retentionDays must be between 1 and 365".into());
        }
        if self.cleanup.run_hour_utc > 23 {
            return Err("cleanup.runHourUtc must be between 0 and 23".into());
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

fn default_max_concurrent_jobs() -> usize {
    3
}

fn default_cleanup_retention_days() -> u16 {
    7
}

fn default_cleanup_hour_utc() -> u8 {
    3
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
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["unknown".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig::default(),
            cleanup: CleanupConfig::default(),
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
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["grok".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig {
                enabled: true,
                poll_seconds: 60,
                default_harness: "codex".into(),
            },
            cleanup: CleanupConfig::default(),
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
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["codex".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig::default(),
            cleanup: CleanupConfig::default(),
        };

        assert!(config.allows_issue_author(Some("TRUSTED-USER")));
        assert!(!config.allows_issue_author(Some("someone-else")));
        assert!(!config.allows_issue_author(None));

        let mut missing = config;
        missing.allowed_issue_authors.clear();
        assert!(missing.validate().is_err());
    }

    #[test]
    fn rejects_empty_cursor_effort_model() {
        let mut config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["cursor".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig::default(),
            cleanup: CleanupConfig::default(),
        };
        config.cursor_effort_models.high.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_concurrency_and_cleanup_bounds() {
        let mut config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["codex".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig::default(),
            cleanup: CleanupConfig::default(),
        };

        assert!(config.validate().is_ok());
        config.max_concurrent_jobs = 0;
        assert!(config.validate().is_err());
        config.max_concurrent_jobs = 3;
        config.cleanup.retention_days = 0;
        assert!(config.validate().is_err());
        config.cleanup.retention_days = 7;
        config.cleanup.run_hour_utc = 24;
        assert!(config.validate().is_err());
    }

    #[test]
    fn deployment_config_enables_parallel_jobs_and_cleanup() {
        let config: Config = serde_json::from_str(include_str!("../deploy/config.json")).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.max_concurrent_jobs, 3);
        assert!(config.cleanup.enabled);
        assert_eq!(config.cleanup.retention_days, 7);
        assert_eq!(config.cleanup.run_hour_utc, 3);
    }

    #[test]
    fn existing_configs_get_safe_scheduler_defaults() {
        let config: Config = serde_json::from_str(
            r#"{
                "listen":"127.0.0.1",
                "port":8787,
                "stateDir":"/tmp/state",
                "workDir":"/tmp/work",
                "allowedHarnesses":["codex"],
                "allowedIssueAuthors":["trusted-user"]
            }"#,
        )
        .unwrap();
        assert_eq!(config.max_concurrent_jobs, 3);
        assert!(!config.cleanup.enabled);
        assert_eq!(config.cleanup.retention_days, 7);
    }
}
