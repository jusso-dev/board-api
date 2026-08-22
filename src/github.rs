use crate::{
    error::ApiError,
    model::{valid_column, validate_repo, Card, CreateCardRequest, Page, Repo, BOARD_COLUMNS},
    util::scrub_log_line,
};
use axum::http::StatusCode;
use serde::Deserialize;
use std::{process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

#[derive(Clone, Default)]
pub struct Github;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepo {
    name_with_owner: String,
    description: Option<String>,
    url: String,
    is_private: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<RawLabel>,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawLabel {
    pub name: String,
}

impl Github {
    pub async fn require_login(&self) -> Result<(), ApiError> {
        self.run(&["auth", "status", "--hostname", "github.com"])
            .await
            .map(|_| ())
            .map_err(|_| {
                ApiError::dependency(
                    "gh_login_required",
                    "GitHub CLI is not authenticated; run `gh auth login` as board",
                )
            })
    }

    pub async fn login(&self) -> Option<String> {
        self.run(&["api", "user", "--jq", ".login"])
            .await
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub async fn list_repos(&self) -> Result<Vec<Repo>, ApiError> {
        self.require_login().await?;
        let output = self
            .run(&[
                "repo",
                "list",
                "--limit",
                "200",
                "--json",
                "nameWithOwner,description,url,isPrivate",
            ])
            .await?;
        let raw: Vec<RawRepo> = serde_json::from_str(&output)
            .map_err(|error| ApiError::internal(format!("invalid gh repository JSON: {error}")))?;
        Ok(raw
            .into_iter()
            .map(|repo| Repo {
                name_with_owner: repo.name_with_owner,
                description: repo.description,
                url: repo.url,
                is_private: repo.is_private,
            })
            .collect())
    }

    pub async fn list_cards(
        &self,
        repo: &str,
        column: Option<&str>,
        page: usize,
        per_page: usize,
    ) -> Result<Page<Card>, ApiError> {
        validate_repo_arg(repo)?;
        if let Some(column) = column {
            validate_column_arg(column)?;
        }
        self.require_login().await?;
        let limit = page
            .checked_mul(per_page)
            .filter(|limit| *limit <= 5_000)
            .ok_or_else(|| {
                ApiError::bad_request("page_too_large", "requested page is too large")
            })?;
        let output = self
            .run(&[
                "issue",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--limit",
                &limit.to_string(),
                "--json",
                "number,title,body,labels,url,createdAt,updatedAt",
            ])
            .await?;
        let mut cards = parse_issues(&output)?;
        if let Some(column) = column {
            cards.retain(|card| card.column.as_deref() == Some(column));
        }
        let start = (page - 1) * per_page;
        let has_more = cards.len() > start.saturating_add(per_page);
        let items = cards.into_iter().skip(start).take(per_page).collect();
        Ok(Page {
            items,
            page,
            per_page,
            has_more,
        })
    }

    pub async fn get_card(&self, repo: &str, number: u64) -> Result<Card, ApiError> {
        validate_repo_arg(repo)?;
        if number == 0 {
            return Err(ApiError::bad_request(
                "invalid_issue",
                "issue number must be greater than zero",
            ));
        }
        self.require_login().await?;
        let output = self
            .run(&[
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                repo,
                "--json",
                "number,title,body,labels,url,createdAt,updatedAt",
            ])
            .await
            .map_err(map_not_found("card"))?;
        parse_issue(&output)
    }

    pub async fn create_card(&self, request: &CreateCardRequest) -> Result<Card, ApiError> {
        validate_repo_arg(&request.repo)?;
        validate_column_arg(&request.column)?;
        let title = request.title.trim();
        if title.is_empty() || title.len() > 256 {
            return Err(ApiError::bad_request(
                "invalid_title",
                "title must contain 1 to 256 bytes",
            ));
        }
        if request.body.len() > 65_536 {
            return Err(ApiError::bad_request(
                "body_too_large",
                "body must not exceed 65536 bytes",
            ));
        }
        self.require_login().await?;
        self.ensure_labels(&request.repo).await?;
        let url = self
            .run(&[
                "issue",
                "create",
                "--repo",
                &request.repo,
                "--title",
                title,
                "--body",
                &request.body,
                "--label",
                &request.column,
            ])
            .await?;
        let number = url
            .trim()
            .rsplit('/')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| ApiError::internal("gh returned an issue URL without a number"))?;
        self.get_card(&request.repo, number).await
    }

    pub async fn move_card(&self, repo: &str, number: u64, column: &str) -> Result<Card, ApiError> {
        validate_repo_arg(repo)?;
        validate_column_arg(column)?;
        self.require_login().await?;
        self.ensure_labels(repo).await?;
        let current = self.get_card(repo, number).await?;
        let mut arguments = vec![
            "issue".to_string(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            repo.into(),
        ];
        for label in current
            .labels
            .iter()
            .filter(|label| BOARD_COLUMNS.contains(&label.as_str()) && label.as_str() != column)
        {
            arguments.push("--remove-label".into());
            arguments.push(label.clone());
        }
        if !current.labels.iter().any(|label| label == column) {
            arguments.push("--add-label".into());
            arguments.push(column.into());
        }
        self.run_owned(&arguments).await?;
        self.get_card(repo, number).await
    }

    pub async fn issue_prompt(&self, repo: &str, number: u64) -> Result<String, ApiError> {
        let card = self.get_card(repo, number).await?;
        Ok(format!(
            "Work GitHub issue #{number}: {}\n\n{}\n\nIssue URL: {}",
            card.title, card.body, card.url
        ))
    }

    pub async fn comment_issue(&self, repo: &str, number: u64, body: &str) -> Result<(), ApiError> {
        self.run(&[
            "issue",
            "comment",
            &number.to_string(),
            "--repo",
            repo,
            "--body",
            body,
        ])
        .await
        .map(|_| ())
    }

    pub async fn existing_pr(&self, repo: &str, branch: &str) -> Result<Option<String>, ApiError> {
        let output = self
            .run(&[
                "pr",
                "list",
                "--repo",
                repo,
                "--head",
                branch,
                "--state",
                "open",
                "--json",
                "url",
                "--jq",
                ".[0].url // \"\"",
            ])
            .await?;
        let value = output.trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }

    pub async fn ensure_labels(&self, repo: &str) -> Result<(), ApiError> {
        const COLORS: [&str; 5] = ["6e7781", "1f883d", "bf8700", "8250df", "0969da"];
        for (label, color) in BOARD_COLUMNS.iter().zip(COLORS) {
            self.run(&[
                "label", "create", label, "--repo", repo, "--color", color, "--force",
            ])
            .await?;
        }
        Ok(())
    }

    pub async fn run(&self, arguments: &[&str]) -> Result<String, ApiError> {
        let owned = arguments
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        self.run_owned(&owned).await
    }

    pub async fn run_owned(&self, arguments: &[String]) -> Result<String, ApiError> {
        let mut command = Command::new("gh");
        command
            .args(arguments)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(90), command.output())
            .await
            .map_err(|_| ApiError::dependency("gh_timeout", "GitHub CLI timed out"))?
            .map_err(|error| {
                ApiError::dependency("gh_unavailable", format!("cannot execute gh: {error}"))
            })?;
        if !output.status.success() {
            let stderr = scrub_log_line(&String::from_utf8_lossy(&output.stderr));
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "gh_failed",
                first_line_or(&stderr, "GitHub CLI command failed"),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn validate_repo_arg(repo: &str) -> Result<(), ApiError> {
    validate_repo(repo)
        .then_some(())
        .ok_or_else(|| ApiError::bad_request("invalid_repo", "repo must be owner/name"))
}

fn validate_column_arg(column: &str) -> Result<(), ApiError> {
    valid_column(column).then_some(()).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_column",
            "column must be one of the five board:* labels",
        )
    })
}

fn parse_issues(output: &str) -> Result<Vec<Card>, ApiError> {
    let issues: Vec<RawIssue> = serde_json::from_str(output)
        .map_err(|error| ApiError::internal(format!("invalid gh issue JSON: {error}")))?;
    Ok(issues.into_iter().map(card_from_raw).collect())
}

fn parse_issue(output: &str) -> Result<Card, ApiError> {
    let issue: RawIssue = serde_json::from_str(output)
        .map_err(|error| ApiError::internal(format!("invalid gh issue JSON: {error}")))?;
    Ok(card_from_raw(issue))
}

fn card_from_raw(issue: RawIssue) -> Card {
    let labels = issue
        .labels
        .into_iter()
        .map(|label| label.name)
        .collect::<Vec<_>>();
    let column = labels
        .iter()
        .find(|label| BOARD_COLUMNS.contains(&label.as_str()))
        .cloned();
    Card {
        number: issue.number,
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        column,
        labels,
        url: issue.url,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
    }
}

fn map_not_found(resource: &'static str) -> impl FnOnce(ApiError) -> ApiError {
    move |error| {
        if error.message.to_ascii_lowercase().contains("not found") {
            ApiError::not_found(resource)
        } else {
            error
        }
    }
}

fn first_line_or(value: &str, fallback: &str) -> String {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback)
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_board_column_from_labels() {
        let card = card_from_raw(RawIssue {
            number: 7,
            title: "Test".into(),
            body: None,
            labels: vec![
                RawLabel { name: "bug".into() },
                RawLabel {
                    name: "board:ready".into(),
                },
            ],
            url: "https://example.invalid/7".into(),
            created_at: "2026-08-22T00:00:00Z".into(),
            updated_at: "2026-08-22T00:00:00Z".into(),
        });
        assert_eq!(card.column.as_deref(), Some("board:ready"));
    }
}
