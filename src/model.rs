use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BOARD_COLUMNS: [&str; 5] = [
    "board:backlog",
    "board:ready",
    "board:running",
    "board:review",
    "board:done",
];
pub const AGENT_LABELS: [&str; 3] = ["agent:grok", "agent:codex", "agent:cursor"];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub ok: bool,
    pub version: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairRequest {
    pub code: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub token: String,
    pub server_id: String,
    pub name: &'static str,
    pub base_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintKeyResponse {
    pub id: String,
    pub token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResponse {
    pub deleted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerResponse {
    pub name: &'static str,
    pub server_id: String,
    pub version: &'static str,
    pub listen: String,
    pub lan_url: String,
    pub tailscale_url: Option<String>,
    pub tailscale_dns: Option<String>,
    pub harnesses: Vec<String>,
    pub gh_login: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateCardRequest {
    pub repo: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub column: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoveCardRequest {
    pub column: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub name_with_owner: String,
    pub description: Option<String>,
    pub url: String,
    pub is_private: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub column: Option<String>,
    pub labels: Vec<String>,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryCard {
    pub repo: String,
    pub card: Card,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewPage {
    pub items: Vec<RepositoryCard>,
    pub page: usize,
    pub per_page: usize,
    pub has_more: bool,
    pub partial: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: usize,
    pub per_page: usize,
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateJobRequest {
    pub repo: String,
    pub issue: u64,
    pub harness: String,
    pub prompt: Option<String>,
    #[serde(default)]
    pub crew: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Cancelling,
    Cancelled,
    Succeeded,
    Failed,
}

impl JobStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Succeeded | Self::Failed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    pub repo: String,
    pub issue: u64,
    pub harness: String,
    pub crew: Vec<String>,
    pub status: JobStatus,
    pub branch: String,
    pub worktree: PathBuf,
    pub pr_url: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    pub timestamp: String,
    pub kind: &'static str,
    pub line: String,
}

pub fn valid_column(column: &str) -> bool {
    BOARD_COLUMNS.contains(&column)
}

pub fn validate_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(name) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return false;
    }
    [owner, name].into_iter().all(|part| {
        part.len() <= 100
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    })
}

pub fn parse_pagination(
    page: Option<&str>,
    per_page: Option<&str>,
) -> Result<(usize, usize), String> {
    let page = page
        .unwrap_or("1")
        .parse::<usize>()
        .map_err(|_| "page must be a positive integer")?;
    let per_page = per_page
        .unwrap_or("25")
        .parse::<usize>()
        .map_err(|_| "perPage must be a positive integer")?;
    if page == 0 {
        return Err("page must be at least 1".into());
    }
    if !(1..=50).contains(&per_page) {
        return Err("perPage must be between 1 and 50".into());
    }
    Ok((page, per_page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repo_names_without_shell_metacharacters() {
        assert!(validate_repo("owner/repo-name_1.0"));
        assert!(!validate_repo("owner/repo/extra"));
        assert!(!validate_repo("owner/$(bad)"));
        assert!(!validate_repo("../repo"));
    }

    #[test]
    fn pagination_is_bounded() {
        assert_eq!(parse_pagination(None, None).unwrap(), (1, 25));
        assert!(parse_pagination(Some("0"), None).is_err());
        assert!(parse_pagination(None, Some("51")).is_err());
    }
}
