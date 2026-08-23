mod auth;
mod automation;
mod config;
mod error;
mod github;
mod jobs;
mod model;
mod util;

use auth::AuthManager;
use automation::AutoRunner;
use axum::{
    body::Body,
    extract::{
        rejection::{JsonRejection, QueryRejection},
        DefaultBodyLimit, Path, Query, Request, State,
    },
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{delete, get, post},
    Json, Router,
};
use config::Config;
use error::ApiError;
use github::Github;
use jobs::JobManager;
use model::{
    parse_pagination, CreateCardRequest, CreateCommentRequest, CreateJobRequest, DeleteResponse,
    HealthResponse, MoveCardRequest, PairRequest, ServerResponse, VERSION,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    convert::Infallible,
    net::{IpAddr, UdpSocket},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, process::Command, signal};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    auth: Arc<AuthManager>,
    github: Github,
    jobs: Arc<JobManager>,
    automation: Arc<AutoRunner>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CardsQuery {
    repo: String,
    column: Option<String>,
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaginationQuery {
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentsQuery {
    repo: String,
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepoQuery {
    repo: String,
}

#[tokio::main]
async fn main() {
    if std::env::args().len() == 2
        && matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V"))
    {
        println!("board-api {VERSION}");
        return;
    }
    if std::env::args().len() != 1 {
        eprintln!("usage: board-api [--version]");
        std::process::exit(2);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("board_api=info,tower_http=info")),
        )
        .compact()
        .init();

    if let Err(error) = run().await {
        tracing::error!(%error, "board-api stopped");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = Arc::new(Config::load()?);
    let host_document = std::env::var("BOARD_API_HOST_DOCUMENT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/home/board/HOST.md"));
    let auth = Arc::new(AuthManager::load_or_create(
        &config.state_dir,
        host_document,
    )?);
    let github = Github::default();
    let job_manager = Arc::new(JobManager::new(Arc::clone(&config), github.clone()).await?);
    let automation = Arc::new(AutoRunner::new(
        Arc::clone(&config),
        github.clone(),
        Arc::clone(&job_manager),
    ));
    let state = AppState {
        config: Arc::clone(&config),
        auth,
        github,
        jobs: job_manager,
        automation: Arc::clone(&automation),
    };

    let protected = Router::new()
        .route("/v1/keys", post(mint_key))
        .route("/v1/keys/{id}", delete(revoke_key))
        .route("/v1/server", get(server))
        .route("/v1/repos", get(repos))
        .route("/v1/overview", get(overview))
        .route("/v1/cards", get(cards).post(create_card))
        .route("/v1/cards/{number}", get(card).patch(move_card))
        .route(
            "/v1/cards/{number}/comments",
            get(comments).post(create_comment),
        )
        .route("/v1/jobs", get(jobs).post(create_job))
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/events", get(job_events))
        .route("/v1/jobs/{id}/cancel", post(cancel_job))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/pair", post(pair))
        .merge(protected)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1_048_576))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let address = config.socket_addr();
    let listener = TcpListener::bind(address)
        .await
        .map_err(|error| format!("cannot bind {address}: {error}"))?;
    tracing::info!(%address, version = VERSION, "board-api listening");
    if automation.enabled() {
        tokio::spawn(automation.run());
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("HTTP server failed: {error}"))
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: VERSION,
    })
}

async fn pair(
    State(state): State<AppState>,
    payload: Result<Json<PairRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request = json_payload(payload)?;
    let info = build_server_response(&state).await;
    let paired = state.auth.pair(&request.code, info.lan_url).await?;
    Ok((StatusCode::CREATED, Json(paired)))
}

async fn mint_key(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok((StatusCode::CREATED, Json(state.auth.mint_key().await?)))
}

async fn revoke_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeleteResponse>, ApiError> {
    state.auth.revoke(&id).await?;
    Ok(Json(DeleteResponse { deleted: true }))
}

async fn server(State(state): State<AppState>) -> Json<ServerResponse> {
    Json(build_server_response(&state).await)
}

async fn repos(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(state.github.list_repos().await?))
}

async fn overview(
    State(state): State<AppState>,
    query: Result<Query<PaginationQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    let (page, per_page) = parse_pagination(query.page.as_deref(), query.per_page.as_deref())
        .map_err(|message| ApiError::bad_request("invalid_pagination", message))?;
    Ok(Json(state.github.overview_cards(page, per_page).await?))
}

async fn cards(
    State(state): State<AppState>,
    query: Result<Query<CardsQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    let (page, per_page) = parse_pagination(query.page.as_deref(), query.per_page.as_deref())
        .map_err(|message| ApiError::bad_request("invalid_pagination", message))?;
    Ok(Json(
        state
            .github
            .list_cards(&query.repo, query.column.as_deref(), page, per_page)
            .await?,
    ))
}

async fn create_card(
    State(state): State<AppState>,
    payload: Result<Json<CreateCardRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request = json_payload(payload)?;
    let card = state.github.create_card(&request).await?;
    state.automation.consider(&request.repo, &card).await;
    Ok((StatusCode::CREATED, Json(card)))
}

async fn card(
    State(state): State<AppState>,
    Path(number): Path<String>,
    query: Result<Query<RepoQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    Ok(Json(
        state
            .github
            .get_card(&query.repo, positive_number(&number)?)
            .await?,
    ))
}

async fn move_card(
    State(state): State<AppState>,
    Path(number): Path<String>,
    query: Result<Query<RepoQuery>, QueryRejection>,
    payload: Result<Json<MoveCardRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    let request = json_payload(payload)?;
    let card = state
        .github
        .move_card(&query.repo, positive_number(&number)?, &request.column)
        .await?;
    state.automation.consider(&query.repo, &card).await;
    Ok(Json(card))
}

async fn comments(
    State(state): State<AppState>,
    Path(number): Path<String>,
    query: Result<Query<CommentsQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    let (page, per_page) = parse_pagination(query.page.as_deref(), query.per_page.as_deref())
        .map_err(|message| ApiError::bad_request("invalid_pagination", message))?;
    Ok(Json(
        state
            .github
            .list_comments(&query.repo, positive_number(&number)?, page, per_page)
            .await?,
    ))
}

async fn create_comment(
    State(state): State<AppState>,
    Path(number): Path<String>,
    query: Result<Query<RepoQuery>, QueryRejection>,
    payload: Result<Json<CreateCommentRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = query_payload(query)?;
    let request = json_payload(payload)?;
    let login = state.github.login().await.ok_or_else(|| {
        ApiError::dependency(
            "gh_login_required",
            "GitHub CLI login is required before posting comments",
        )
    })?;
    if !state.config.allows_issue_author(Some(&login)) {
        return Err(ApiError::forbidden(
            "comment_author_not_allowed",
            "the server GitHub identity is not in allowedIssueAuthors",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .github
                .create_comment(&query.repo, positive_number(&number)?, &request.body)
                .await?,
        ),
    ))
}

async fn jobs(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(state.jobs.list().await?))
}

async fn create_job(
    State(state): State<AppState>,
    payload: Result<Json<CreateJobRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request = json_payload(payload)?;
    let card = state.github.get_card(&request.repo, request.issue).await?;
    if let Err(error) = require_allowed_issue_author(&state.config, &card) {
        tracing::warn!(
            repo = %request.repo,
            issue = request.issue,
            author = card.author_login.as_deref().unwrap_or("<missing>"),
            "job rejected because issue author is not allowed"
        );
        return Err(error);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(state.jobs.create(request).await?),
    ))
}

async fn job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(state.jobs.get(&id).await?))
}

async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(state.jobs.cancel(&id).await?))
}

async fn job_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (history, receiver) = state.jobs.events(&id).await?;
    let replay = tokio_stream::iter(history.into_iter().map(event_to_sse));
    let live = BroadcastStream::new(receiver).filter_map(|result| result.ok().map(event_to_sse));
    Ok(Sse::new(replay.chain(live)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

fn event_to_sse(event: model::JobEvent) -> Result<Event, Infallible> {
    let kind = event.kind;
    let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
    Ok(Event::default().event(kind).data(data))
}

async fn require_auth(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match token {
        Some(token) if state.auth.authorize(token).await => next.run(request).await,
        _ => ApiError::unauthorized().into_response(),
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("route")
}

async fn build_server_response(state: &AppState) -> ServerResponse {
    let lan_ip = lan_ip().unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let (tailscale_ip, tailscale_dns) = tailscale_identity().await;
    let tailscale_url = tailscale_ip.map(|ip| format!("http://{ip}:{}", state.config.port));
    let harnesses = installed_harnesses(&state.config.allowed_harnesses).await;
    ServerResponse {
        name: "board",
        server_id: state.auth.server_id().await,
        version: VERSION,
        listen: state.config.socket_addr().to_string(),
        lan_url: format!("http://{lan_ip}:{}", state.config.port),
        tailscale_url,
        tailscale_dns,
        harnesses,
        gh_login: state.github.login().await,
    }
}

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:80").ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}

async fn tailscale_identity() -> (Option<IpAddr>, Option<String>) {
    let ip = command_output("tailscale", &["ip", "-4"])
        .await
        .and_then(|value| value.trim().parse::<IpAddr>().ok());
    let dns = command_output("tailscale", &["status", "--json"])
        .await
        .and_then(|output| serde_json::from_str::<Value>(&output).ok())
        .and_then(|value| {
            value
                .get("Self")
                .and_then(|value| value.get("DNSName"))
                .and_then(Value::as_str)
                .map(|value| value.trim_end_matches('.').to_string())
        })
        .filter(|value| !value.is_empty());
    (ip, dns)
}

async fn installed_harnesses(allowed: &[String]) -> Vec<String> {
    let mut installed = Vec::new();
    for harness in allowed {
        let binary = if harness == "cursor" {
            "cursor-agent"
        } else {
            harness.as_str()
        };
        if Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
        {
            installed.push(harness.clone());
        }
    }
    installed
}

async fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload
        .map(|Json(value)| value)
        .map_err(|error| ApiError::bad_request("invalid_json", error.body_text()))
}

fn query_payload<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(value)| value)
        .map_err(|error| ApiError::bad_request("invalid_query", error.body_text()))
}

fn positive_number(value: &str) -> Result<u64, ApiError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| {
            ApiError::bad_request("invalid_issue", "issue number must be a positive integer")
        })
}

fn require_allowed_issue_author(config: &Config, card: &model::Card) -> Result<(), ApiError> {
    config
        .allows_issue_author(card.author_login.as_deref())
        .then_some(())
        .ok_or_else(|| {
            ApiError::forbidden(
                "issue_author_not_allowed",
                "issue author is not allowed to start board jobs",
            )
        })
}

async fn shutdown_signal() {
    let control_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = control_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{AutoRunConfig, Config},
        model::Card,
    };
    use axum::http::StatusCode;
    use std::path::PathBuf;

    const OPENAPI_OPERATIONS: &[(&str, &str)] = &[
        ("/v1/health", "get"),
        ("/v1/pair", "post"),
        ("/v1/keys", "post"),
        ("/v1/keys/{id}", "delete"),
        ("/v1/server", "get"),
        ("/v1/repos", "get"),
        ("/v1/cards", "get"),
        ("/v1/cards", "post"),
        ("/v1/cards/{number}", "get"),
        ("/v1/cards/{number}", "patch"),
        ("/v1/jobs", "get"),
        ("/v1/jobs", "post"),
        ("/v1/jobs/{id}", "get"),
        ("/v1/jobs/{id}/events", "get"),
        ("/v1/jobs/{id}/cancel", "post"),
    ];

    #[test]
    fn openapi_documents_every_route_and_operation() {
        let document = include_str!("../openapi.yaml");
        for (path, method) in OPENAPI_OPERATIONS {
            let path_marker = format!("  {path}:");
            let start = document
                .find(&path_marker)
                .unwrap_or_else(|| panic!("OpenAPI missing {path}"));
            let remainder = &document[start + path_marker.len()..];
            let end = remainder.find("\n  /v1/").unwrap_or(remainder.len());
            let section = &remainder[..end];
            assert!(
                section.contains(&format!("\n    {method}:")),
                "OpenAPI missing {method} {path}"
            );
        }
    }

    #[test]
    fn issue_number_is_positive() {
        assert!(super::positive_number("1").is_ok());
        assert!(super::positive_number("0").is_err());
        assert!(super::positive_number("bad").is_err());
    }

    #[test]
    fn job_author_policy_rejects_foreign_and_missing_authors() {
        let config = Config {
            listen: "0.0.0.0".parse().unwrap(),
            port: 8787,
            state_dir: PathBuf::from("/tmp/state"),
            work_dir: PathBuf::from("/tmp/work"),
            allowed_harnesses: vec!["codex".into()],
            allowed_issue_authors: vec!["trusted-user".into()],
            auto_run: AutoRunConfig::default(),
        };
        let card = |author: Option<&str>| Card {
            number: 1,
            author_login: author.map(str::to_string),
            title: "Test".into(),
            body: String::new(),
            column: Some("board:ready".into()),
            labels: vec!["board:ready".into()],
            url: "https://github.com/example/repo/issues/1".into(),
            created_at: "2026-08-22T00:00:00Z".into(),
            updated_at: "2026-08-22T00:00:00Z".into(),
        };

        assert!(super::require_allowed_issue_author(&config, &card(Some("TRUSTED-USER"))).is_ok());
        for author in [Some("someone-else"), None] {
            let error = super::require_allowed_issue_author(&config, &card(author)).unwrap_err();
            assert_eq!(error.status, StatusCode::FORBIDDEN);
            assert_eq!(error.code, "issue_author_not_allowed");
        }
    }
}
