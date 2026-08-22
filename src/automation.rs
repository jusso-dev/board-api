use crate::{
    config::Config,
    error::ApiError,
    github::{Github, ReadyCard},
    jobs::JobManager,
    model::{Card, CreateJobRequest, JobRecord, AGENT_LABELS},
};
use std::{cmp::Ordering, sync::Arc};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::time::{sleep, Duration};

#[derive(Clone)]
pub struct AutoRunner {
    config: Arc<Config>,
    github: Github,
    jobs: Arc<JobManager>,
}

impl AutoRunner {
    pub fn new(config: Arc<Config>, github: Github, jobs: Arc<JobManager>) -> Self {
        Self {
            config,
            github,
            jobs,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.auto_run.enabled
    }

    pub async fn run(self: Arc<Self>) {
        tracing::info!(
            poll_seconds = self.config.auto_run.poll_seconds,
            default_harness = %self.config.auto_run.default_harness,
            labels = "agent:grok,agent:codex,agent:cursor",
            "automatic ready-card runner enabled"
        );
        loop {
            match self.scan_once().await {
                Ok(count) if count > 0 => {
                    tracing::info!(started_jobs = count, "automatic ready-card scan complete");
                }
                Ok(_) => tracing::debug!("automatic ready-card scan found no new work"),
                Err(error) => tracing::warn!(
                    code = error.code,
                    message = %error.message,
                    "automatic ready-card scan failed"
                ),
            }
            sleep(Duration::from_secs(self.config.auto_run.poll_seconds)).await;
        }
    }

    pub async fn consider(&self, repo: &str, card: &Card) {
        if !self.enabled() {
            return;
        }
        let jobs = match self.jobs.list().await {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(
                    repo,
                    issue = card.number,
                    message = %error.message,
                    "cannot inspect jobs for automatic ready card"
                );
                return;
            }
        };
        self.consider_with_jobs(repo, card, &jobs).await;
    }

    async fn scan_once(&self) -> Result<usize, ApiError> {
        let candidates = self.github.ready_cards().await?;
        let mut jobs = self.jobs.list().await?;
        let mut started = 0;
        for ReadyCard { repo, card } in candidates {
            if let Some(job) = self.consider_with_jobs(&repo, &card, &jobs).await {
                jobs.push(job);
                started += 1;
            }
        }
        Ok(started)
    }

    async fn consider_with_jobs(
        &self,
        repo: &str,
        card: &Card,
        jobs: &[JobRecord],
    ) -> Option<JobRecord> {
        if card.column.as_deref() != Some("board:ready") || !should_start(repo, card, jobs) {
            return None;
        }
        let harness = match selected_harness(
            &card.labels,
            &self.config.auto_run.default_harness,
            &self.config.allowed_harnesses,
        ) {
            Ok(harness) => harness,
            Err(message) => {
                tracing::warn!(repo, issue = card.number, %message, "ready card skipped");
                return None;
            }
        };
        let request = CreateJobRequest {
            repo: repo.into(),
            issue: card.number,
            harness: harness.clone(),
            prompt: None,
            crew: Vec::new(),
        };
        match self.jobs.create(request).await {
            Ok(job) => {
                tracing::info!(
                    job_id = %job.id,
                    repo,
                    issue = card.number,
                    %harness,
                    "automatic board job queued"
                );
                Some(job)
            }
            Err(error) if error.code == "repo_job_running" => {
                tracing::debug!(
                    repo,
                    issue = card.number,
                    "ready card waiting for repository job"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    repo,
                    issue = card.number,
                    %harness,
                    code = error.code,
                    message = %error.message,
                    "automatic board job could not start"
                );
                None
            }
        }
    }
}

fn selected_harness(
    labels: &[String],
    default_harness: &str,
    allowed_harnesses: &[String],
) -> Result<String, String> {
    let selectors = labels
        .iter()
        .filter(|label| label.starts_with("agent:"))
        .collect::<Vec<_>>();
    let harness = match selectors.as_slice() {
        [] => default_harness,
        [label] if AGENT_LABELS.contains(&label.as_str()) => label
            .strip_prefix("agent:")
            .expect("known agent label has prefix"),
        [label] => return Err(format!("unsupported agent selector label {label}")),
        _ => return Err("use exactly one agent:* selector label".into()),
    };
    if !allowed_harnesses.iter().any(|allowed| allowed == harness) {
        return Err(format!(
            "agent harness {harness} is not allowed by this server"
        ));
    }
    Ok(harness.into())
}

fn should_start(repo: &str, card: &Card, jobs: &[JobRecord]) -> bool {
    let latest = jobs
        .iter()
        .filter(|job| job.repo.eq_ignore_ascii_case(repo) && job.issue == card.number)
        .max_by(|left, right| compare_timestamps(&left.created_at, &right.created_at));
    let Some(job) = latest else {
        return true;
    };
    if !job.status.is_terminal() {
        return false;
    }
    let Some(finished_at) = job.finished_at.as_deref() else {
        return false;
    };
    is_after(&card.updated_at, finished_at).unwrap_or(false)
}

fn compare_timestamps(left: &str, right: &str) -> Ordering {
    match (parse_timestamp(left), parse_timestamp(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

fn is_after(left: &str, right: &str) -> Option<bool> {
    Some(parse_timestamp(left)? > parse_timestamp(right)?)
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::JobStatus;
    use std::path::PathBuf;

    fn card(labels: &[&str], updated_at: &str) -> Card {
        Card {
            number: 7,
            title: "Test".into(),
            body: String::new(),
            column: Some("board:ready".into()),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            url: "https://github.com/example/repo/issues/7".into(),
            created_at: "2026-08-22T00:00:00Z".into(),
            updated_at: updated_at.into(),
        }
    }

    fn job(status: JobStatus, finished_at: Option<&str>) -> JobRecord {
        JobRecord {
            id: "00000000-0000-4000-8000-000000000007".into(),
            repo: "example/repo".into(),
            issue: 7,
            harness: "codex".into(),
            crew: Vec::new(),
            status,
            branch: "board/7-00000000".into(),
            worktree: PathBuf::from("/tmp/work"),
            pr_url: None,
            created_at: "2026-08-22T00:01:00Z".into(),
            started_at: Some("2026-08-22T00:01:01Z".into()),
            finished_at: finished_at.map(str::to_string),
            error: None,
        }
    }

    #[test]
    fn selector_uses_default_or_exact_agent_label() {
        let allowed = vec!["grok".into(), "codex".into(), "cursor".into()];
        assert_eq!(
            selected_harness(&card(&["board:ready"], "").labels, "codex", &allowed).unwrap(),
            "codex"
        );
        assert_eq!(
            selected_harness(
                &card(&["board:ready", "agent:grok"], "").labels,
                "codex",
                &allowed
            )
            .unwrap(),
            "grok"
        );
    }

    #[test]
    fn selector_rejects_ambiguous_or_unknown_agent_labels() {
        let allowed = vec!["grok".into(), "codex".into(), "cursor".into()];
        assert!(selected_harness(
            &card(&["board:ready", "agent:grok", "agent:cursor"], "").labels,
            "codex",
            &allowed
        )
        .is_err());
        assert!(selected_harness(
            &card(&["board:ready", "agent:unknown"], "").labels,
            "codex",
            &allowed
        )
        .is_err());
    }

    #[test]
    fn terminal_job_is_not_retried_until_issue_changes() {
        let completed = job(JobStatus::Failed, Some("2026-08-22T00:02:00.500Z"));
        assert!(!should_start(
            "example/repo",
            &card(&["board:ready"], "2026-08-22T00:02:00Z"),
            std::slice::from_ref(&completed)
        ));
        assert!(should_start(
            "example/repo",
            &card(&["board:ready"], "2026-08-22T00:03:00Z"),
            &[completed]
        ));
    }

    #[test]
    fn active_job_prevents_duplicate_pickup() {
        assert!(!should_start(
            "example/repo",
            &card(&["board:ready"], "2026-08-22T00:03:00Z"),
            &[job(JobStatus::Running, None)]
        ));
    }
}
