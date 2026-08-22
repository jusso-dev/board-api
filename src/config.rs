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
        Ok(())
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.listen, self.port)
    }
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
        };
        assert!(config.validate().is_err());
    }
}
