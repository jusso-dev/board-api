use crate::{
    config::Config,
    error::ApiError,
    github::Github,
    model::{validate_repo, CreateJobRequest, JobEvent, JobRecord, JobStatus},
    util::{iso_now, scrub_log_line, write_private_json},
};
use axum::http::StatusCode;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{broadcast, Mutex, RwLock},
    time::{sleep, Duration},
};
use uuid::Uuid;

const MAX_EVENT_HISTORY: usize = 500;

#[derive(Clone)]
pub struct JobManager {
    config: Arc<Config>,
    github: Github,
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
            config,
            github,
            running_by_repo: Arc::new(Mutex::new(HashMap::new())),
            process_groups: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            senders: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(HashMap::new())),
        };
        manager.mark_interrupted_jobs().await?;
        Ok(manager)
    }

    pub async fn create(
        self: &Arc<Self>,
        request: CreateJobRequest,
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
                    format!("repository already has running job {existing}"),
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
            self.running_by_repo
                .lock()
                .await
                .remove(&request.repo.to_ascii_lowercase());
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
        self.running_by_repo
            .lock()
            .await
            .remove(&record.repo.to_ascii_lowercase());
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
            let (program, arguments) = harness_command(&harness, &record.worktree, &prompt)?;
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
) -> Result<(&'static str, Vec<String>), String> {
    let workspace = worktree.to_string_lossy().to_string();
    match harness {
        "codex" => Ok((
            "codex",
            vec![
                "exec".into(),
                "--approve-for-me".into(),
                "--color".into(),
                "never".into(),
                "--cd".into(),
                workspace,
                prompt.into(),
            ],
        )),
        "grok" => Ok((
            "grok",
            vec![
                "--single".into(),
                prompt.into(),
                "--cwd".into(),
                workspace,
                "--permission-mode".into(),
                "auto".into(),
                "--output-format".into(),
                "streaming-json".into(),
                "--no-subagents".into(),
            ],
        )),
        "cursor" => Ok((
            "cursor-agent",
            vec![
                "--print".into(),
                "--force".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--workspace".into(),
                workspace,
                prompt.into(),
            ],
        )),
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

    fn request() -> CreateJobRequest {
        CreateJobRequest {
            repo: "owner/repo".into(),
            issue: 12,
            harness: "codex".into(),
            prompt: None,
            crew: vec!["cursor".into(), "codex".into()],
        }
    }

    #[test]
    fn crew_is_sequential_and_deduplicated() {
        assert_eq!(harness_sequence(&request()), ["codex", "cursor"]);
    }

    #[test]
    fn codex_command_uses_safe_automatic_review() {
        let (program, arguments) =
            harness_command("codex", Path::new("/tmp/work"), "do it").unwrap();
        assert_eq!(program, "codex");
        assert!(arguments
            .iter()
            .any(|argument| argument == "--approve-for-me"));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("dangerously")));
    }

    #[test]
    fn grok_command_allows_headless_tool_use_without_bypass_mode() {
        let (program, arguments) =
            harness_command("grok", Path::new("/tmp/work"), "do it").unwrap();
        assert_eq!(program, "grok");
        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments[0] == "--permission-mode" && arguments[1] == "auto" }));
        assert!(!arguments
            .iter()
            .any(|argument| { argument == "--always-approve" || argument == "bypassPermissions" }));
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
