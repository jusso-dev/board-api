use crate::{
    error::ApiError,
    model::{
        valid_column, validate_repo, Card, CreateCardRequest, OverviewPage, Page, Repo,
        RepositoryCard, AGENT_LABELS, BOARD_COLUMNS,
    },
    util::scrub_log_line,
};
use axum::http::StatusCode;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

#[derive(Clone, Default)]
pub struct Github;

#[derive(Debug, Deserialize)]
struct RawRepo {
    #[serde(rename = "full_name")]
    name_with_owner: String,
    description: Option<String>,
    #[serde(rename = "html_url")]
    url: String,
    #[serde(rename = "private")]
    is_private: bool,
}

#[derive(Debug, Deserialize)]
struct RawRepoAccess {
    #[serde(rename = "full_name")]
    name_with_owner: String,
    owner: String,
    owner_type: String,
    push: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawIssue {
    pub number: u64,
    pub author: Option<RawAuthor>,
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<RawLabel>,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawAuthor {
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawLabel {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSearchIssue {
    #[serde(flatten)]
    issue: RawIssue,
    repository_url: String,
}

#[derive(Clone, Debug)]
pub struct ReadyCard {
    pub repo: String,
    pub card: Card,
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
                "api",
                "--paginate",
                "/user/repos?per_page=100&affiliation=owner%2Ccollaborator%2Corganization_member&sort=full_name&direction=asc",
                "--jq",
                ".[] | {full_name, description, html_url, private}",
            ])
            .await?;
        parse_repo_stream(&output)
    }

    pub async fn ready_cards(&self) -> Result<Vec<ReadyCard>, ApiError> {
        self.search_board_cards(&["board:ready"])
            .await
            .map(|(cards, _)| cards)
    }

    pub async fn overview_cards(
        &self,
        page: usize,
        per_page: usize,
    ) -> Result<OverviewPage, ApiError> {
        let (mut cards, partial) = self.search_board_cards(&BOARD_COLUMNS).await?;
        cards.sort_by(|left, right| {
            right
                .card
                .updated_at
                .cmp(&left.card.updated_at)
                .then_with(|| left.repo.cmp(&right.repo))
                .then_with(|| left.card.number.cmp(&right.card.number))
        });

        let start = (page - 1)
            .checked_mul(per_page)
            .filter(|start| *start <= 5_000)
            .ok_or_else(|| {
                ApiError::bad_request("page_too_large", "requested page is too large")
            })?;
        let has_more = cards.len() > start.saturating_add(per_page);
        let items = cards
            .into_iter()
            .skip(start)
            .take(per_page)
            .map(|entry| RepositoryCard {
                repo: entry.repo,
                card: entry.card,
            })
            .collect();

        Ok(OverviewPage {
            items,
            page,
            per_page,
            has_more,
            partial,
        })
    }

    async fn search_board_cards(
        &self,
        labels: &[&str],
    ) -> Result<(Vec<ReadyCard>, bool), ApiError> {
        let access = self.list_repo_access().await?;
        let mut pushable = HashMap::new();
        let mut owners = BTreeMap::new();
        for repo in access.into_iter().filter(|repo| repo.push) {
            pushable.insert(
                repo.name_with_owner.to_ascii_lowercase(),
                repo.name_with_owner,
            );
            owners.insert(repo.owner, repo.owner_type);
        }

        let mut cards = Vec::new();
        let mut seen = HashSet::new();
        let mut successful_searches = 0;
        let mut last_error = None;
        for (owner, owner_type) in owners {
            let qualifier = match owner_type.as_str() {
                "Organization" => "org",
                "User" => "user",
                _ => continue,
            };
            let query = board_search_query(labels, qualifier, &owner);
            let arguments = vec![
                "api".into(),
                "-X".into(),
                "GET".into(),
                "--paginate".into(),
                "/search/issues".into(),
                "-f".into(),
                format!("q={query}"),
                "-f".into(),
                "per_page=100".into(),
                "-f".into(),
                "sort=created".into(),
                "-f".into(),
                "order=asc".into(),
                "--jq".into(),
                ".items[] | {number, author: (if .user == null then null else {login: .user.login} end), title, body, labels, url: .html_url, createdAt: .created_at, updatedAt: .updated_at, repositoryUrl: .repository_url}".into(),
            ];
            let output = match self.run_owned(&arguments).await {
                Ok(output) => {
                    successful_searches += 1;
                    output
                }
                Err(error) => {
                    tracing::warn!(
                        %owner,
                        code = error.code,
                        message = %error.message,
                        "GitHub board-card owner search failed"
                    );
                    last_error = Some(error);
                    continue;
                }
            };
            for issue in parse_search_issue_stream(&output)? {
                let Some(found_repo) = repo_from_repository_url(&issue.repository_url) else {
                    continue;
                };
                let Some(repo) = pushable.get(&found_repo.to_ascii_lowercase()).cloned() else {
                    continue;
                };
                let key = format!("{}#{}", repo.to_ascii_lowercase(), issue.issue.number);
                if seen.insert(key) {
                    cards.push(ReadyCard {
                        repo,
                        card: card_from_raw(issue.issue),
                    });
                }
            }
        }
        if successful_searches == 0 {
            if let Some(error) = last_error {
                return Err(error);
            }
        }
        Ok((cards, last_error.is_some()))
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
                "number,author,title,body,labels,url,createdAt,updatedAt",
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
                "number,author,title,body,labels,url,createdAt,updatedAt",
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
        const AGENT_COLORS: [&str; 3] = ["0f1419", "10a37f", "6f42c1"];
        for (label, color) in AGENT_LABELS.iter().zip(AGENT_COLORS) {
            self.run(&[
                "label",
                "create",
                label,
                "--repo",
                repo,
                "--color",
                color,
                "--description",
                "Select the board-api coding harness for board:ready issues",
                "--force",
            ])
            .await?;
        }
        Ok(())
    }

    async fn list_repo_access(&self) -> Result<Vec<RawRepoAccess>, ApiError> {
        self.require_login().await?;
        let output = self
            .run(&[
                "api",
                "--paginate",
                "/user/repos?per_page=100&affiliation=owner%2Ccollaborator%2Corganization_member&sort=full_name&direction=asc",
                "--jq",
                ".[] | {full_name, owner: .owner.login, owner_type: .owner.type, push: (.permissions.push // false)}",
            ])
            .await?;
        parse_repo_access_stream(&output)
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

fn parse_repo_stream(output: &str) -> Result<Vec<Repo>, ApiError> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str::<RawRepo>(line)
                .map(|repo| Repo {
                    name_with_owner: repo.name_with_owner,
                    description: repo.description,
                    url: repo.url,
                    is_private: repo.is_private,
                })
                .map_err(|error| {
                    ApiError::internal(format!(
                        "invalid gh repository JSON on line {}: {error}",
                        index + 1
                    ))
                })
        })
        .collect()
}

fn parse_repo_access_stream(output: &str) -> Result<Vec<RawRepoAccess>, ApiError> {
    parse_json_lines(output, "repository access")
}

fn parse_search_issue_stream(output: &str) -> Result<Vec<RawSearchIssue>, ApiError> {
    parse_json_lines(output, "GitHub search issue")
}

fn parse_json_lines<T>(output: &str, kind: &str) -> Result<Vec<T>, ApiError>
where
    T: for<'de> Deserialize<'de>,
{
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|error| {
                ApiError::internal(format!(
                    "invalid {kind} JSON on line {}: {error}",
                    index + 1
                ))
            })
        })
        .collect()
}

fn repo_from_repository_url(url: &str) -> Option<String> {
    let repo = url.strip_prefix("https://api.github.com/repos/")?;
    validate_repo(repo).then(|| repo.to_string())
}

fn board_search_query(labels: &[&str], qualifier: &str, owner: &str) -> String {
    let labels = labels
        .iter()
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("is:issue is:open label:{labels} {qualifier}:{owner}")
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
        author_login: issue.author.map(|author| author.login),
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
    fn parses_repositories_across_affiliations_and_pages() {
        let repos = parse_repo_stream(
            r#"{"full_name":"jusso-dev/board-api","description":"Personal repository","html_url":"https://github.com/jusso-dev/board-api","private":false}
{"full_name":"example-org/operations","description":null,"html_url":"https://github.com/example-org/operations","private":true}"#,
        )
        .unwrap();

        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name_with_owner, "jusso-dev/board-api");
        assert_eq!(repos[1].name_with_owner, "example-org/operations");
        assert!(repos[1].is_private);
    }

    #[test]
    fn parses_board_column_from_labels() {
        let card = card_from_raw(RawIssue {
            number: 7,
            author: Some(RawAuthor {
                login: "jusso-dev".into(),
            }),
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
        assert_eq!(card.author_login.as_deref(), Some("jusso-dev"));
    }

    #[test]
    fn parses_push_access_used_to_scope_automation() {
        let repos = parse_repo_access_stream(
            r#"{"full_name":"example-org/app","owner":"example-org","owner_type":"Organization","push":true}
{"full_name":"outside/read-only","owner":"outside","owner_type":"Organization","push":false}"#,
        )
        .unwrap();

        assert_eq!(repos.len(), 2);
        assert!(repos[0].push);
        assert!(!repos[1].push);
    }

    #[test]
    fn parses_ready_issue_search_results() {
        let issues = parse_search_issue_stream(
            r#"{"number":9,"author":{"login":"jusso-dev"},"title":"Ship it","body":"Implement it","labels":[{"name":"board:ready"},{"name":"agent:grok"}],"url":"https://github.com/example-org/app/issues/9","createdAt":"2026-08-22T00:00:00Z","updatedAt":"2026-08-22T01:00:00Z","repositoryUrl":"https://api.github.com/repos/example-org/app"}"#,
        )
        .unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.number, 9);
        assert_eq!(
            issues[0]
                .issue
                .author
                .as_ref()
                .map(|author| author.login.as_str()),
            Some("jusso-dev")
        );
        assert_eq!(
            repo_from_repository_url(&issues[0].repository_url).as_deref(),
            Some("example-org/app")
        );
    }

    #[test]
    fn missing_issue_author_is_preserved_for_fail_closed_policy() {
        let mut issues = parse_search_issue_stream(
            r#"{"number":10,"author":null,"title":"Ghost author","body":null,"labels":[{"name":"board:ready"}],"url":"https://github.com/example-org/app/issues/10","createdAt":"2026-08-22T00:00:00Z","updatedAt":"2026-08-22T01:00:00Z","repositoryUrl":"https://api.github.com/repos/example-org/app"}"#,
        )
        .unwrap();

        let card = card_from_raw(issues.remove(0).issue);
        assert!(card.author_login.is_none());
    }

    #[test]
    fn builds_one_or_label_search_for_every_board_column() {
        assert_eq!(
            board_search_query(&BOARD_COLUMNS, "org", "example-org"),
            "is:issue is:open label:\"board:backlog\",\"board:ready\",\"board:running\",\"board:review\",\"board:done\" org:example-org"
        );
    }
}
