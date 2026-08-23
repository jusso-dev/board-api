use crate::{
    config::{Config, CursorEffortModels},
    error::ApiError,
    github::Github,
    model::{validate_repo, CreateJobRequest, JobEvent, JobRecord, JobStatus, ReasoningEffort},
    util::{iso_now, scrub_log_line, write_private_json},
};
use axum::http::StatusCode;
use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use time::{
    format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime,
    PrimitiveDateTime, Time,
};
use time_tz::{timezones, OffsetDateTimeExt, OffsetResult, PrimitiveDateTimeExt, Tz};
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{broadcast, Mutex, RwLock, Semaphore},
    time::{sleep, Duration},
};
use uuid::Uuid;

const MAX_EVENT_HISTORY: usize = 500;

#[derive(Debug, Default, PartialEq, Eq)]
struct CleanupReport {
    removed: usize,
    skipped_active: usize,
    skipped_recent: usize,
    skipped_invalid: usize,
    missing: usize,
}

#[derive(Clone)]
pub struct JobManager {
    config: Arc<Config>,
    github: Github,
    job_slots: Arc<Semaphore>,
    running_by_repo: Arc<Mutex<HashMap<String, String>>>,
    process_groups: Arc<Mutex<HashMap<String, u32>>>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    senders: Arc<RwLock<HashMap<String, broadcast::Sender<JobEvent>>>>,
    history: Arc<RwLock<HashMap<String, Vec<JobEvent>>>>,
}

impl JobManager {
    pub async fn new(config: Arc<Config>, github: Github) -> Result<Self, String> {
        fs::create_dir_all(config.state_dir.join("jobs"))
            .await
            .map_err(|error| format!("cannot create job state directory: {error}"))?;
        fs::create_dir_all(&config.work_dir)
            .await
            .map_err(|error| format!("cannot create work directory: {error}"))?;
        let manager = Self {
            job_slots: Arc::new(Semaphore::new(config.max_concurrent_jobs)),
            config,
            github,
            running_by_repo: Arc::new(Mutex::new(HashMap::new())),
            process_groups: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            senders: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(HashMap::new())),
        };
        manager.mark_interrupted_jobs().await?;
        tracing::info!(
            max_concurrent_jobs = manager.config.max_concurrent_jobs,
            "job scheduler configured"
        );
        Ok(manager)
    }

    pub fn cleanup_enabled(&self) -> bool {
        self.config.cleanup.enabled
    }

    pub async fn run_cleanup_loop(self: Arc<Self>) {
        let timezone = timezones::get_by_name(&self.config.cleanup.timezone)
            .expect("cleanup timezone was validated");
        tracing::info!(
            retention_days = self.config.cleanup.retention_days,
            timezone = %self.config.cleanup.timezone,
            run_hour_local = self.config.cleanup.run_hour_local,
            "job worktree cleanup enabled"
        );
        loop {
            let now = OffsetDateTime::now_utc();
            match self.cleanup_once_at(now).await {
                Ok(report) => tracing::info!(
                    removed = report.removed,
                    skipped_active = report.skipped_active,
                    skipped_recent = report.skipped_recent,
                    skipped_invalid = report.skipped_invalid,
                    missing = report.missing,
                    "job worktree cleanup sweep complete"
                ),
                Err(error) => tracing::warn!(
                    message = %scrub_log_line(&error),
                    "job worktree cleanup sweep failed closed"
                ),
            }
            let delay = seconds_until_cleanup(
                OffsetDateTime::now_utc(),
                self.config.cleanup.run_hour_local,
                timezone,
            );
            sleep(Duration::from_secs(delay)).await;
        }
    }

    pub async fn create(
        self: &Arc<Self>,
        request: CreateJobRequest,
        effort: Option<ReasoningEffort>,
    ) -> Result<JobRecord, ApiError> {
        validate_job_request(&self.config, &request)?;
        self.github.require_login().await?;

        let sequence = harness_sequence(&request);
        for harness in &sequence {
            if !self.harness_authenticated(harness).await? {
                return Err(ApiError::dependency(
                    "harness_login_required",
                    format!("{harness} is not authenticated as board"),
                ));
            }
        }

        let repo_key = request.repo.to_ascii_lowercase();
        let id = Uuid::new_v4().to_string();
        {
            let mut running = self.running_by_repo.lock().await;
            if let Some(existing) = running.get(&repo_key) {
                return Err(ApiError::conflict(
                    "repo_job_running",
                    format!("repository already has queued or running job {existing}"),
                ));
            }
            running.insert(repo_key, id.clone());
        }

        let (owner, repository) = request.repo.split_once('/').expect("validated repo");
        let short_id = &id[..8];
        let record = JobRecord {
            id: id.clone(),
            repo: request.repo.clone(),
            issue: request.issue,
            harness: request.harness.clone(),
            crew: request.crew.clone(),
            effort,
            status: JobStatus::Queued,
            branch: format!("board/{}-{short_id}", request.issue),
            worktree: self.config.work_dir.join(owner).join(repository).join(&id),
            pr_url: None,
            created_at: iso_now(),
            started_at: None,
            finished_at: None,
            error: None,
        };
        if let Err(error) = self.persist(&record) {
            self.release_repo(&request.repo, &id).await;
            return Err(ApiError::internal(error));
        }
        self.ensure_sender(&id).await;
        self.send_event(&id, "status", "queued").await;

        let manager = Arc::clone(self);
        let response = record.clone();
        tokio::spawn(async move {
            manager.run_job(record, request.prompt).await;
        });
        Ok(response)
    }

    pub async fn list(&self) -> Result<Vec<JobRecord>, ApiError> {
        let mut directory = fs::read_dir(self.config.state_dir.join("jobs"))
            .await
            .map_err(|error| ApiError::internal(format!("cannot list jobs: {error}")))?;
        let mut jobs = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| ApiError::internal(format!("cannot list jobs: {error}")))?
        {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path())
                .await
                .map_err(|error| ApiError::internal(format!("cannot read job: {error}")))?;
            let job: JobRecord = serde_json::from_slice(&bytes)
                .map_err(|error| ApiError::internal(format!("invalid job JSON: {error}")))?;
            jobs.push(job);
        }
        jobs.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(jobs)
    }

    pub async fn get(&self, id: &str) -> Result<JobRecord, ApiError> {
        validate_job_id(id)?;
        let path = self.job_path(id);
        let bytes = fs::read(&path).await.map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ApiError::not_found("job"),
            _ => ApiError::internal(format!("cannot read {}: {error}", path.display())),
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| ApiError::internal(format!("invalid job JSON: {error}")))
    }

    pub async fn cancel(self: &Arc<Self>, id: &str) -> Result<JobRecord, ApiError> {
        let mut record = self.get(id).await?;
        if record.status.is_terminal() {
            return Err(ApiError::conflict(
                "job_not_running",
                "job has already finished",
            ));
        }
        self.cancelled.lock().await.insert(id.to_string());
        if record.status == JobStatus::Queued {
            record.status = JobStatus::Cancelled;
            record.finished_at = Some(iso_now());
            self.persist(&record).map_err(ApiError::internal)?;
            self.send_event(id, "status", "cancelled").await;
            self.release_repo(&record.repo, &record.id).await;
            return Ok(record);
        }
        record.status = JobStatus::Cancelling;
        self.persist(&record).map_err(ApiError::internal)?;
        self.send_event(id, "status", "cancelling").await;

        if let Some(process_group) = self.process_groups.lock().await.get(id).copied() {
            signal_process_group(process_group, "-TERM").await;
            let manager = Arc::clone(self);
            let id = id.to_string();
            tokio::spawn(async move {
                sleep(Duration::from_secs(5)).await;
                if let Some(process_group) = manager.process_groups.lock().await.get(&id).copied() {
                    signal_process_group(process_group, "-KILL").await;
                }
            });
        }
        Ok(record)
    }

    pub async fn events(
        &self,
        id: &str,
    ) -> Result<(Vec<JobEvent>, broadcast::Receiver<JobEvent>), ApiError> {
        self.get(id).await?;
        let sender = self.ensure_sender(id).await;
        let history = self
            .history
            .read()
            .await
            .get(id)
            .cloned()
            .unwrap_or_default();
        Ok((history, sender.subscribe()))
    }

    async fn run_job(self: Arc<Self>, mut record: JobRecord, extra_prompt: Option<String>) {
        let _permit = match Arc::clone(&self.job_slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                record.status = JobStatus::Failed;
                record.finished_at = Some(iso_now());
                record.error = Some("job scheduler stopped before this job could start".into());
                let _ = self.persist(&record);
                self.send_event(&record.id, "status", "failed").await;
                self.release_repo(&record.repo, &record.id).await;
                return;
            }
        };
        if self.cancelled.lock().await.remove(&record.id) {
            record.status = JobStatus::Cancelled;
            record.finished_at = Some(iso_now());
            record.error = None;
            let _ = self.persist(&record);
            self.send_event(&record.id, "status", "cancelled").await;
            self.release_repo(&record.repo, &record.id).await;
            return;
        }
        record.status = JobStatus::Running;
        record.started_at = Some(iso_now());
        let _ = self.persist(&record);
        self.send_event(&record.id, "status", "running").await;

        let result = self.execute_job(&mut record, extra_prompt).await;
        let was_cancelled = self.cancelled.lock().await.remove(&record.id);
        match result {
            Ok(()) if was_cancelled => {
                record.status = JobStatus::Cancelled;
                record.error = None;
            }
            Ok(()) => {
                record.status = JobStatus::Succeeded;
                record.error = None;
            }
            Err(_) if was_cancelled => {
                record.status = JobStatus::Cancelled;
                record.error = None;
            }
            Err(error) => {
                record.status = JobStatus::Failed;
                record.error = Some(scrub_log_line(&error));
                let _ = self
                    .github
                    .move_card(&record.repo, record.issue, "board:ready")
                    .await;
                let comment = match record.pr_url.as_deref() {
                    Some(pr_url) => format!(
                        "Board job `{}` opened {pr_url}, but did not finish cleanly and returned this issue to `board:ready`. Open the Board app job record for details.",
                        record.id
                    ),
                    None => format!(
                        "Board job `{}` did not open a pull request and returned this issue to `board:ready`. Open the Board app job record for details.",
                        record.id
                    ),
                };
                let _ = self
                    .github
                    .comment_issue(&record.repo, record.issue, &comment)
                    .await;
            }
        }
        record.finished_at = Some(iso_now());
        let _ = self.persist(&record);
        self.send_event(
            &record.id,
            "status",
            match record.status {
                JobStatus::Cancelled => "cancelled",
                JobStatus::Succeeded => "succeeded",
                _ => "failed",
            },
        )
        .await;
        self.process_groups.lock().await.remove(&record.id);
        self.release_repo(&record.repo, &record.id).await;
    }

    async fn execute_job(
        self: &Arc<Self>,
        record: &mut JobRecord,
        extra_prompt: Option<String>,
    ) -> Result<(), String> {
        self.ensure_not_cancelled(&record.id).await?;
        self.github
            .ensure_labels(&record.repo)
            .await
            .map_err(|error| error.message)?;
        self.github
            .move_card(&record.repo, record.issue, "board:running")
            .await
            .map_err(|error| error.message)?;

        if let Some(parent) = record.worktree.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("cannot create workspace parent: {error}"))?;
        }
        let worktree = record.worktree.to_string_lossy().to_string();
        self.run_command(
            &record.id,
            "gh",
            &[
                "repo".into(),
                "clone".into(),
                record.repo.clone(),
                worktree.clone(),
                "--".into(),
                "--filter=blob:none".into(),
            ],
            None,
        )
        .await?;
        self.run_command(
            &record.id,
            "git",
            &["checkout".into(), "-b".into(), record.branch.clone()],
            Some(&record.worktree),
        )
        .await?;

        let issue_prompt = self
            .github
            .issue_prompt(
                &record.repo,
                record.issue,
                &self.config.allowed_issue_authors,
            )
            .await
            .map_err(|error| error.message)?;
        let prompt = format!(
            "{issue_prompt}\n\n{}\n\nWork only in this repository. Implement the issue, run relevant tests, and leave the working tree ready for review. Never print credentials or tokens.",
            extra_prompt.unwrap_or_default()
        );
        let request = CreateJobRequest {
            repo: record.repo.clone(),
            issue: record.issue,
            harness: record.harness.clone(),
            prompt: None,
            crew: record.crew.clone(),
        };
        for harness in harness_sequence(&request) {
            self.send_event(&record.id, "status", &format!("running {harness}"))
                .await;
            let (program, arguments) = harness_command(
                &harness,
                &record.worktree,
                &prompt,
                record.effort,
                &self.config.cursor_effort_models,
            )?;
            self.run_command(&record.id, program, &arguments, Some(&record.worktree))
                .await?;
        }

        let changes = self
            .run_command(
                &record.id,
                "git",
                &["status".into(), "--porcelain".into()],
                Some(&record.worktree),
            )
            .await?;
        if !changes.trim().is_empty() {
            self.run_command(
                &record.id,
                "git",
                &["add".into(), "-A".into()],
                Some(&record.worktree),
            )
            .await?;
            self.run_command(
                &record.id,
                "git",
                &[
                    "-c".into(),
                    "user.name=board-api".into(),
                    "-c".into(),
                    "user.email=board@localhost".into(),
                    "commit".into(),
                    "-m".into(),
                    format!(
                        "Resolve #{} via board job {}",
                        record.issue,
                        &record.id[..8]
                    ),
                ],
                Some(&record.worktree),
            )
            .await?;
        }

        let ahead = self
            .run_command(
                &record.id,
                "git",
                &[
                    "rev-list".into(),
                    "--count".into(),
                    "origin/HEAD..HEAD".into(),
                ],
                Some(&record.worktree),
            )
            .await?
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        require_repository_change(ahead)?;
        self.run_command(
            &record.id,
            "git",
            &[
                "push".into(),
                "-u".into(),
                "origin".into(),
                record.branch.clone(),
            ],
            Some(&record.worktree),
        )
        .await?;
        let pr_url = if let Some(url) = self
            .github
            .existing_pr(&record.repo, &record.branch)
            .await
            .map_err(|error| error.message)?
        {
            url
        } else {
            self.run_command(
                &record.id,
                "gh",
                &[
                    "pr".into(),
                    "create".into(),
                    "--repo".into(),
                    record.repo.clone(),
                    "--head".into(),
                    record.branch.clone(),
                    "--fill".into(),
                    "--body".into(),
                    format!("Closes #{}", record.issue),
                ],
                Some(&record.worktree),
            )
            .await?
            .trim()
            .to_string()
        };
        if pr_url.is_empty() {
            return Err("GitHub CLI did not return a pull request URL".into());
        }
        record.pr_url = Some(pr_url.clone());
        self.persist(record)?;
        if let Err(error) = self
            .github
            .comment_issue(
                &record.repo,
                record.issue,
                &format!("Board job `{}` opened {pr_url}", record.id),
            )
            .await
        {
            tracing::warn!(
                job_id = %record.id,
                repo = %record.repo,
                issue = record.issue,
                message = %error.message,
                "pull request opened but issue comment failed"
            );
            self.send_event(
                &record.id,
                "log",
                "Pull request opened, but the GitHub issue comment failed.",
            )
            .await;
        }
        self.github
            .move_card(&record.repo, record.issue, "board:review")
            .await
            .map_err(|error| error.message)?;
        self.persist(record)?;
        Ok(())
    }

    async fn run_command(
        self: &Arc<Self>,
        job_id: &str,
        program: &str,
        arguments: &[String],
        directory: Option<&Path>,
    ) -> Result<String, String> {
        self.ensure_not_cancelled(job_id).await?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start {program}: {error}"))?;
        let process_group = child
            .id()
            .ok_or_else(|| format!("{program} started without a process id"))?;
        self.process_groups
            .lock()
            .await
            .insert(job_id.to_string(), process_group);

        let stdout = child.stdout.take().expect("stdout configured");
        let stderr = child.stderr.take().expect("stderr configured");
        let stdout_reader = spawn_reader(Arc::clone(self), job_id.to_string(), stdout, "log");
        let stderr_reader = spawn_reader(Arc::clone(self), job_id.to_string(), stderr, "log");
        let status = child
            .wait()
            .await
            .map_err(|error| format!("cannot wait for {program}: {error}"))?;
        self.process_groups.lock().await.remove(job_id);
        let stdout_lines = stdout_reader
            .await
            .map_err(|error| format!("cannot collect {program} stdout: {error}"))??;
        let stderr_lines = stderr_reader
            .await
            .map_err(|error| format!("cannot collect {program} stderr: {error}"))??;
        self.ensure_not_cancelled(job_id).await?;
        if !status.success() {
            let message = stderr_lines
                .iter()
                .chain(stdout_lines.iter())
                .find(|line| !line.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("{program} exited with {status}"));
            return Err(scrub_log_line(&message));
        }
        Ok(stdout_lines.join("\n"))
    }

    async fn harness_authenticated(&self, harness: &str) -> Result<bool, ApiError> {
        let (program, arguments): (&str, &[&str]) = match harness {
            "codex" => ("codex", &["login", "status"]),
            "grok" => ("grok", &["models"]),
            "cursor" => ("cursor-agent", &["status"]),
            _ => return Ok(false),
        };
        let output = Command::new(program)
            .args(arguments)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                ApiError::dependency(
                    "harness_unavailable",
                    format!("cannot execute {program}: {error}"),
                )
            })?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase();
        Ok(output.status.success()
            && !combined.contains("not authenticated")
            && !combined.contains("not logged in")
            && !combined.contains("login required"))
    }

    async fn ensure_not_cancelled(&self, id: &str) -> Result<(), String> {
        if self.cancelled.lock().await.contains(id) {
            Err("job cancelled".into())
        } else {
            Ok(())
        }
    }

    async fn ensure_sender(&self, id: &str) -> broadcast::Sender<JobEvent> {
        if let Some(sender) = self.senders.read().await.get(id).cloned() {
            return sender;
        }
        let mut senders = self.senders.write().await;
        senders
            .entry(id.to_string())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    pub async fn send_event(&self, id: &str, kind: &'static str, line: &str) {
        let event = JobEvent {
            timestamp: iso_now(),
            kind,
            line: scrub_log_line(line),
        };
        let mut histories = self.history.write().await;
        let history = histories.entry(id.to_string()).or_default();
        history.push(event.clone());
        if history.len() > MAX_EVENT_HISTORY {
            history.drain(..history.len() - MAX_EVENT_HISTORY);
        }
        drop(histories);
        let _ = self.ensure_sender(id).await.send(event);
    }

    fn persist(&self, record: &JobRecord) -> Result<(), String> {
        write_private_json(&self.job_path(&record.id), record)
    }

    fn job_path(&self, id: &str) -> PathBuf {
        self.config
            .state_dir
            .join("jobs")
            .join(format!("{id}.json"))
    }

    async fn release_repo(&self, repo: &str, id: &str) {
        let key = repo.to_ascii_lowercase();
        let mut running = self.running_by_repo.lock().await;
        if running.get(&key).is_some_and(|existing| existing == id) {
            running.remove(&key);
        }
    }

    async fn cleanup_once_at(&self, now: OffsetDateTime) -> Result<CleanupReport, String> {
        let jobs = self
            .list()
            .await
            .map_err(|error| format!("cannot inspect job records: {}", error.message))?;
        let active_repos = jobs
            .iter()
            .filter(|job| !job.status.is_terminal())
            .map(|job| job.repo.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let cutoff = now - TimeDuration::days(i64::from(self.config.cleanup.retention_days));
        let mut report = CleanupReport::default();

        for job in jobs.into_iter().filter(|job| job.status.is_terminal()) {
            let Some(finished_at) = job
                .finished_at
                .as_deref()
                .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
            else {
                report.skipped_invalid += 1;
                continue;
            };
            if finished_at > cutoff {
                report.skipped_recent += 1;
                continue;
            }

            let repo_key = job.repo.to_ascii_lowercase();
            if active_repos.contains(&repo_key) {
                report.skipped_active += 1;
                continue;
            }
            let Some(expected_worktree) = expected_worktree(&self.config.work_dir, &job) else {
                report.skipped_invalid += 1;
                continue;
            };
            if job.worktree != expected_worktree {
                report.skipped_invalid += 1;
                continue;
            }

            let running = self.running_by_repo.lock().await;
            if running.contains_key(&repo_key) {
                report.skipped_active += 1;
                continue;
            }
            let managed_tree_is_safe = match managed_tree_is_safe(&self.config.work_dir, &job).await
            {
                Ok(is_safe) => is_safe,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    report.missing += 1;
                    continue;
                }
                Err(error) => {
                    report.skipped_invalid += 1;
                    tracing::warn!(
                        message = %scrub_log_line(&error.to_string()),
                        "cleanup could not inspect one managed worktree"
                    );
                    continue;
                }
            };
            if !managed_tree_is_safe {
                report.skipped_invalid += 1;
                continue;
            }
            match fs::remove_dir_all(&expected_worktree).await {
                Ok(()) => report.removed += 1,
                Err(error) if error.kind() == ErrorKind::NotFound => report.missing += 1,
                Err(error) => {
                    report.skipped_invalid += 1;
                    tracing::warn!(
                        message = %scrub_log_line(&error.to_string()),
                        "cleanup could not remove one managed worktree"
                    );
                }
            }
        }
        Ok(report)
    }

    async fn mark_interrupted_jobs(&self) -> Result<(), String> {
        let mut directory = fs::read_dir(self.config.state_dir.join("jobs"))
            .await
            .map_err(|error| format!("cannot inspect existing jobs: {error}"))?;
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| format!("cannot inspect existing jobs: {error}"))?
        {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path())
                .await
                .map_err(|error| format!("cannot read existing job: {error}"))?;
            let mut record: JobRecord = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid existing job JSON: {error}"))?;
            if reconcile_persisted_job(&mut record, &iso_now()) {
                self.persist(&record)?;
            }
        }
        Ok(())
    }
}

fn expected_worktree(work_dir: &Path, job: &JobRecord) -> Option<PathBuf> {
    if !validate_repo(&job.repo) {
        return None;
    }
    let uuid = Uuid::parse_str(&job.id).ok()?;
    if uuid.to_string() != job.id {
        return None;
    }
    let (owner, repository) = job.repo.split_once('/')?;
    Some(work_dir.join(owner).join(repository).join(&job.id))
}

async fn managed_tree_is_safe(work_dir: &Path, job: &JobRecord) -> Result<bool, std::io::Error> {
    let (owner, repository) = job.repo.split_once('/').expect("validated repository");
    let owner_dir = work_dir.join(owner);
    let repository_dir = owner_dir.join(repository);
    let worktree = repository_dir.join(&job.id);
    for path in [work_dir, &owner_dir, &repository_dir, &worktree] {
        let metadata = fs::symlink_metadata(path).await?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn seconds_until_cleanup(now: OffsetDateTime, run_hour_local: u8, timezone: &Tz) -> u64 {
    let local_now = now.to_timezone(timezone);
    let run_time = Time::from_hms(run_hour_local, 0, 0).expect("cleanup hour was validated");
    let run_date = if local_now.time() < run_time {
        local_now.date()
    } else {
        local_now
            .date()
            .next_day()
            .expect("cleanup schedule date is representable")
    };
    let mut local_target = PrimitiveDateTime::new(run_date, run_time);
    let target = loop {
        match local_target.assume_timezone(timezone) {
            OffsetResult::Some(value) => break value,
            OffsetResult::Ambiguous(first, _) => break first,
            OffsetResult::None => local_target += TimeDuration::hours(1),
        }
    };
    (target - now).whole_seconds().max(1) as u64
}

fn validate_job_request(config: &Config, request: &CreateJobRequest) -> Result<(), ApiError> {
    if !validate_repo(&request.repo) {
        return Err(ApiError::bad_request(
            "invalid_repo",
            "repo must be owner/name",
        ));
    }
    if request.issue == 0 {
        return Err(ApiError::bad_request(
            "invalid_issue",
            "issue number must be greater than zero",
        ));
    }
    if request
        .prompt
        .as_ref()
        .map(|value| value.len())
        .unwrap_or(0)
        > 32_768
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "prompt_too_large",
            "prompt must not exceed 32768 bytes",
        ));
    }
    if request.crew.len() > 5 {
        return Err(ApiError::bad_request(
            "crew_too_large",
            "crew must contain at most five harnesses",
        ));
    }
    for harness in harness_sequence(request) {
        if !config
            .allowed_harnesses
            .iter()
            .any(|allowed| allowed == &harness)
        {
            return Err(ApiError::bad_request(
                "invalid_harness",
                format!("harness {harness} is not allowed"),
            ));
        }
    }
    Ok(())
}

fn harness_sequence(request: &CreateJobRequest) -> Vec<String> {
    let mut sequence = vec![request.harness.clone()];
    for harness in &request.crew {
        if !sequence.contains(harness) {
            sequence.push(harness.clone());
        }
    }
    sequence
}

fn harness_command(
    harness: &str,
    worktree: &Path,
    prompt: &str,
    effort: Option<ReasoningEffort>,
    cursor_effort_models: &CursorEffortModels,
) -> Result<(&'static str, Vec<String>), String> {
    let workspace = worktree.to_string_lossy().to_string();
    match harness {
        "codex" => {
            let mut arguments = vec![
                "exec".into(),
                "--approve-for-me".into(),
                "--color".into(),
                "never".into(),
                "--cd".into(),
                workspace,
                prompt.into(),
            ];
            if let Some(effort) = effort {
                arguments.splice(
                    1..1,
                    [
                        "--config".into(),
                        format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                    ],
                );
            }
            Ok(("codex", arguments))
        }
        "grok" => {
            let mut arguments = vec![
                "--single".into(),
                prompt.into(),
                "--cwd".into(),
                workspace,
                "--permission-mode".into(),
                "auto".into(),
                "--output-format".into(),
                "streaming-json".into(),
                "--no-subagents".into(),
            ];
            if let Some(effort) = effort {
                arguments.splice(0..0, ["--reasoning-effort".into(), effort.as_str().into()]);
            }
            Ok(("grok", arguments))
        }
        "cursor" => {
            let mut arguments = vec![
                "--print".into(),
                "--force".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--workspace".into(),
                workspace,
                prompt.into(),
            ];
            if let Some(effort) = effort {
                arguments.splice(
                    0..0,
                    [
                        "--model".into(),
                        cursor_effort_models.model_for(effort).into(),
                    ],
                );
            }
            Ok(("cursor-agent", arguments))
        }
        _ => Err(format!("unsupported harness {harness}")),
    }
}

fn validate_job_id(id: &str) -> Result<(), ApiError> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| ApiError::bad_request("invalid_job_id", "job id must be a UUID"))
}

fn require_repository_change(ahead: u64) -> Result<(), String> {
    if ahead == 0 {
        Err(
            "agent finished without repository changes; no commit or pull request was created"
                .into(),
        )
    } else {
        Ok(())
    }
}

fn reconcile_persisted_job(record: &mut JobRecord, now: &str) -> bool {
    if !record.status.is_terminal() {
        record.status = JobStatus::Failed;
        record.finished_at = Some(now.into());
        record.error = Some("board-api restarted while job was active".into());
        return true;
    }
    if record.status == JobStatus::Succeeded && record.pr_url.is_none() {
        record.status = JobStatus::Failed;
        record.error = Some(
            "legacy job reported success without a pull request; delivery is unverified".into(),
        );
        return true;
    }
    false
}

fn spawn_reader<R>(
    manager: Arc<JobManager>,
    job_id: String,
    reader: R,
    kind: &'static str,
) -> tokio::task::JoinHandle<Result<Vec<String>, String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut collected = Vec::new();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|error| format!("cannot read child output: {error}"))?
        {
            let line = scrub_log_line(&line);
            manager.send_event(&job_id, kind, &line).await;
            collected.push(line);
        }
        Ok(collected)
    })
}

async fn signal_process_group(process_group: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([signal, "--", &format!("-{process_group}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AutoRunConfig, CleanupConfig};

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("board-api-{name}-{}", Uuid::new_v4()))
    }

    fn test_config(root: &Path) -> Config {
        Config {
            listen: "127.0.0.1".parse().unwrap(),
            port: 8787,
            state_dir: root.join("state"),
            work_dir: root.join("work"),
            max_concurrent_jobs: 3,
            allowed_harnesses: vec!["codex".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            cursor_effort_models: CursorEffortModels::default(),
            auto_run: AutoRunConfig::default(),
            cleanup: CleanupConfig {
                enabled: true,
                retention_days: 7,
                timezone: "Australia/Sydney".into(),
                run_hour_local: 3,
            },
        }
    }

    fn job_record(
        config: &Config,
        id: &str,
        repo: &str,
        status: JobStatus,
        finished_at: Option<&str>,
    ) -> JobRecord {
        let (owner, repository) = repo.split_once('/').unwrap();
        JobRecord {
            id: id.into(),
            repo: repo.into(),
            issue: 7,
            harness: "codex".into(),
            crew: Vec::new(),
            effort: None,
            status,
            branch: format!("board/7-{}", &id[..8]),
            worktree: config.work_dir.join(owner).join(repository).join(id),
            pr_url: None,
            created_at: "2026-08-01T00:00:00Z".into(),
            started_at: Some("2026-08-01T00:00:01Z".into()),
            finished_at: finished_at.map(str::to_string),
            error: None,
        }
    }

    async fn create_worktree(path: &Path) {
        fs::create_dir_all(path).await.unwrap();
        fs::write(path.join("source.rs"), b"fn main() {}\n")
            .await
            .unwrap();
    }

    fn request() -> CreateJobRequest {
        CreateJobRequest {
            repo: "owner/repo".into(),
            issue: 12,
            harness: "codex".into(),
            prompt: None,
            crew: vec!["cursor".into(), "codex".into()],
        }
    }

    #[tokio::test]
    async fn scheduler_has_multiple_bounded_worker_slots() {
        let root = temp_root("concurrency");
        let manager = JobManager::new(Arc::new(test_config(&root)), Github::default())
            .await
            .unwrap();

        assert_eq!(manager.job_slots.available_permits(), 3);
        let first = Arc::clone(&manager.job_slots).try_acquire_owned().unwrap();
        let second = Arc::clone(&manager.job_slots).try_acquire_owned().unwrap();
        let third = Arc::clone(&manager.job_slots).try_acquire_owned().unwrap();
        assert!(Arc::clone(&manager.job_slots).try_acquire_owned().is_err());
        drop(first);
        assert_eq!(manager.job_slots.available_permits(), 1);
        drop((second, third));

        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn an_old_job_cannot_release_a_newer_repo_claim() {
        let root = temp_root("repo-claim");
        let manager = JobManager::new(Arc::new(test_config(&root)), Github::default())
            .await
            .unwrap();
        manager
            .running_by_repo
            .lock()
            .await
            .insert("owner/repo".into(), "new-job".into());

        manager.release_repo("OWNER/REPO", "old-job").await;
        assert_eq!(
            manager
                .running_by_repo
                .lock()
                .await
                .get("owner/repo")
                .map(String::as_str),
            Some("new-job")
        );
        manager.release_repo("owner/repo", "new-job").await;
        assert!(manager.running_by_repo.lock().await.is_empty());

        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_removes_only_expired_terminal_idle_worktrees() {
        let root = temp_root("cleanup");
        let config = Arc::new(test_config(&root));
        let manager = JobManager::new(Arc::clone(&config), Github::default())
            .await
            .unwrap();
        let expired = "00000000-0000-4000-8000-000000000001";
        let active_old = "00000000-0000-4000-8000-000000000002";
        let active_job = "00000000-0000-4000-8000-000000000003";
        let recent = "00000000-0000-4000-8000-000000000004";
        let invalid = "00000000-0000-4000-8000-000000000005";
        let linked = "00000000-0000-4000-8000-000000000006";

        let records = [
            job_record(
                &config,
                expired,
                "idle/repo",
                JobStatus::Succeeded,
                Some("2026-08-01T00:00:02Z"),
            ),
            job_record(
                &config,
                active_old,
                "active/repo",
                JobStatus::Failed,
                Some("2026-08-01T00:00:02Z"),
            ),
            job_record(&config, active_job, "active/repo", JobStatus::Queued, None),
            job_record(
                &config,
                recent,
                "recent/repo",
                JobStatus::Cancelled,
                Some("2026-08-22T00:00:02Z"),
            ),
        ];
        for record in &records {
            create_worktree(&record.worktree).await;
            manager.persist(record).unwrap();
        }
        let outside = root.join("outside-worktree");
        create_worktree(&outside).await;
        let mut invalid_record = job_record(
            &config,
            invalid,
            "invalid/repo",
            JobStatus::Failed,
            Some("2026-08-01T00:00:02Z"),
        );
        invalid_record.worktree = outside.clone();
        manager.persist(&invalid_record).unwrap();
        let linked_owner = root.join("linked-owner");
        let linked_target = linked_owner.join("repo").join(linked);
        create_worktree(&linked_target).await;
        std::os::unix::fs::symlink(&linked_owner, config.work_dir.join("linked")).unwrap();
        let linked_record = job_record(
            &config,
            linked,
            "linked/repo",
            JobStatus::Failed,
            Some("2026-08-01T00:00:02Z"),
        );
        manager.persist(&linked_record).unwrap();

        let now = OffsetDateTime::parse("2026-08-23T12:00:00Z", &Rfc3339).unwrap();
        let report = manager.cleanup_once_at(now).await.unwrap();

        assert_eq!(
            report,
            CleanupReport {
                removed: 1,
                skipped_active: 1,
                skipped_recent: 1,
                skipped_invalid: 2,
                missing: 0,
            }
        );
        assert!(!config.work_dir.join("idle/repo").join(expired).exists());
        assert!(config
            .work_dir
            .join("active/repo")
            .join(active_old)
            .exists());
        assert!(config
            .work_dir
            .join("active/repo")
            .join(active_job)
            .exists());
        assert!(config.work_dir.join("recent/repo").join(recent).exists());
        assert!(outside.exists());
        assert!(linked_target.exists());
        assert!(manager.job_path(expired).exists());

        fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn nightly_cleanup_delay_uses_sydney_time_year_round() {
        let timezone = timezones::get_by_name("Australia/Sydney").unwrap();
        let winter_before = OffsetDateTime::parse("2026-08-23T16:30:00Z", &Rfc3339).unwrap();
        let winter_after = OffsetDateTime::parse("2026-08-23T17:30:00Z", &Rfc3339).unwrap();
        let summer_before = OffsetDateTime::parse("2026-12-23T15:30:00Z", &Rfc3339).unwrap();
        let summer_after = OffsetDateTime::parse("2026-12-23T16:30:00Z", &Rfc3339).unwrap();

        assert_eq!(seconds_until_cleanup(winter_before, 3, timezone), 1_800);
        assert_eq!(seconds_until_cleanup(winter_after, 3, timezone), 84_600);
        assert_eq!(seconds_until_cleanup(summer_before, 3, timezone), 1_800);
        assert_eq!(seconds_until_cleanup(summer_after, 3, timezone), 84_600);
    }

    #[test]
    fn nightly_cleanup_delay_handles_sydney_dst_boundaries() {
        let timezone = timezones::get_by_name("Australia/Sydney").unwrap();
        let spring_forward = OffsetDateTime::parse("2026-10-03T15:59:30Z", &Rfc3339).unwrap();
        let fall_back = OffsetDateTime::parse("2026-04-04T15:30:00Z", &Rfc3339).unwrap();

        assert_eq!(seconds_until_cleanup(spring_forward, 3, timezone), 30);
        assert_eq!(seconds_until_cleanup(fall_back, 3, timezone), 5_400);
    }

    #[test]
    fn crew_is_sequential_and_deduplicated() {
        assert_eq!(harness_sequence(&request()), ["codex", "cursor"]);
    }

    #[test]
    fn codex_command_uses_safe_automatic_review() {
        let (program, arguments) = harness_command(
            "codex",
            Path::new("/tmp/work"),
            "do it",
            None,
            &CursorEffortModels::default(),
        )
        .unwrap();
        assert_eq!(program, "codex");
        assert!(arguments
            .iter()
            .any(|argument| argument == "--approve-for-me"));
        assert!(!arguments.iter().any(|argument| argument == "--config"));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("dangerously")));
    }

    #[test]
    fn grok_command_allows_headless_tool_use_without_bypass_mode() {
        let (program, arguments) = harness_command(
            "grok",
            Path::new("/tmp/work"),
            "do it",
            None,
            &CursorEffortModels::default(),
        )
        .unwrap();
        assert_eq!(program, "grok");
        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments[0] == "--permission-mode" && arguments[1] == "auto" }));
        assert!(!arguments
            .iter()
            .any(|argument| argument == "--reasoning-effort"));
        assert!(!arguments
            .iter()
            .any(|argument| { argument == "--always-approve" || argument == "bypassPermissions" }));
    }

    #[test]
    fn cursor_command_preserves_default_model_without_effort() {
        let (program, arguments) = harness_command(
            "cursor",
            Path::new("/tmp/work"),
            "do it",
            None,
            &CursorEffortModels::default(),
        )
        .unwrap();
        assert_eq!(program, "cursor-agent");
        assert!(!arguments.iter().any(|argument| argument == "--model"));
    }

    #[test]
    fn effort_is_translated_for_each_harness() {
        let cursor_models = CursorEffortModels::default();
        let (_, codex) = harness_command(
            "codex",
            Path::new("/tmp/work"),
            "do it",
            Some(ReasoningEffort::Xhigh),
            &cursor_models,
        )
        .unwrap();
        assert!(codex.windows(2).any(|arguments| {
            arguments[0] == "--config" && arguments[1] == "model_reasoning_effort=\"xhigh\""
        }));

        let (_, grok) = harness_command(
            "grok",
            Path::new("/tmp/work"),
            "do it",
            Some(ReasoningEffort::High),
            &cursor_models,
        )
        .unwrap();
        assert!(grok
            .windows(2)
            .any(|arguments| { arguments[0] == "--reasoning-effort" && arguments[1] == "high" }));

        let (_, cursor) = harness_command(
            "cursor",
            Path::new("/tmp/work"),
            "do it",
            Some(ReasoningEffort::Medium),
            &cursor_models,
        )
        .unwrap();
        assert!(cursor.windows(2).any(|arguments| {
            arguments[0] == "--model" && arguments[1] == "gpt-5.6-sol-medium"
        }));
        assert_eq!(codex.last().map(String::as_str), Some("do it"));
        assert_eq!(cursor.last().map(String::as_str), Some("do it"));
        assert_eq!(grok[2..4], ["--single", "do it"]);
    }

    #[test]
    fn zero_commit_run_cannot_report_success() {
        assert!(require_repository_change(0).is_err());
        assert!(require_repository_change(1).is_ok());
    }

    #[test]
    fn legacy_success_without_pull_request_is_reconciled() {
        let mut record = JobRecord {
            id: "00000000-0000-4000-8000-000000000007".into(),
            repo: "owner/repo".into(),
            issue: 7,
            harness: "grok".into(),
            crew: Vec::new(),
            effort: None,
            status: JobStatus::Succeeded,
            branch: "board/7-00000000".into(),
            worktree: PathBuf::from("/tmp/work"),
            pr_url: None,
            created_at: "2026-08-22T00:00:00Z".into(),
            started_at: Some("2026-08-22T00:00:01Z".into()),
            finished_at: Some("2026-08-22T00:00:02Z".into()),
            error: None,
        };

        assert!(reconcile_persisted_job(&mut record, "2026-08-22T01:00:00Z"));
        assert_eq!(record.status, JobStatus::Failed);
        assert_eq!(record.finished_at.as_deref(), Some("2026-08-22T00:00:02Z"));
        assert!(record.error.as_deref().unwrap().contains("unverified"));
    }
}
