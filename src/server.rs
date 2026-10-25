use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agentscm::model::Revision;
use agentscm::store::Engine;
use agentscm::{Error, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http_body::Body as HttpBody;
use serde::Serialize;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[cfg(unix)]
mod explorer;
#[cfg(unix)]
mod front_door;

const MAX_CONCURRENCY: usize = 256;
const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIFF_LIMIT: usize = 10_000;
const MAX_SKILL_WORKFLOW_BYTES: usize = 4 * 1024 * 1024;
const RESPONSE_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct AppState {
    data_dir: PathBuf,
    engine: Engine,
    work: Arc<Semaphore>,
    /// Git checkout for collision warnings, chosen by the operator at startup.
    collisions: Option<Arc<PathBuf>>,
    /// Native workspace whose timeline `/v1/local/workspace` reads, chosen by the operator.
    timeline: Option<Arc<PathBuf>>,
    #[cfg(unix)]
    explorer: Option<Arc<agentscm::repo_explorer::Explorer>>,
    /// Set by `--front-door`: every connected checkout, chosen per request.
    #[cfg(unix)]
    front_door: Option<Arc<front_door::FrontDoor>>,
}

/// Token and browser origins guarding `/v1/local`, fixed at startup.
struct LocalAccess {
    token: Option<String>,
    origins: Vec<HeaderValue>,
}

const READ_TOKEN_ENV: &str = "AGENTSCM_SERVE_READ_TOKEN";
const MIN_READ_TOKEN_BYTES: usize = 32;

/// Bounds the serialized report before it is returned.
const MAX_COLLISIONS_BYTES: usize = 4 * 1024 * 1024;
/// Bounds how long one scan holds a work slot; the report says when it was cut short.
const COLLISIONS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Serialize)]
struct RevisionResponse {
    id: String,
    revision: Revision,
}

struct ReadQuery {
    path: String,
}

struct DiffQuery {
    left: String,
    right: String,
    limit: usize,
}

struct ResponseGuard {
    inner: Body,
    pending: Option<Bytes>,
    _permit: OwnedSemaphorePermit,
}

pub struct ServeOptions {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub public: bool,
    pub max_concurrency: usize,
    pub collisions: Option<PathBuf>,
    pub timeline: Option<PathBuf>,
    pub browser_origins: Vec<String>,
    pub explorer_repo: Option<PathBuf>,
    pub explorer_repository: Option<String>,
    pub explorer_spool: Option<PathBuf>,
    /// Serve the Hub page and every connected checkout on loopback (`agentscm open`).
    pub front_door: bool,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let ServeOptions {
        data_dir,
        bind,
        public,
        max_concurrency,
        collisions,
        timeline,
        browser_origins,
        explorer_repo,
        explorer_repository,
        explorer_spool,
        front_door,
    } = options;
    if front_door
        && (public
            || collisions.is_some()
            || timeline.is_some()
            || !browser_origins.is_empty()
            || explorer_repo.is_some()
            || explorer_spool.is_some()
            || !bind.ip().is_loopback())
    {
        return Err(Error::Invalid(
            "--front-door serves this computer's connected checkouts on loopback and takes no other source flags".into(),
        ));
    }
    #[cfg(not(unix))]
    if front_door {
        return Err(Error::Invalid("the front door needs a Unix host".into()));
    }
    if !bind.ip().is_loopback() && !public {
        return Err(Error::Invalid(
            "binding a non-loopback address requires --public".into(),
        ));
    }
    if max_concurrency == 0 || max_concurrency > MAX_CONCURRENCY {
        return Err(Error::Limit(format!(
            "max concurrency must be between 1 and {MAX_CONCURRENCY}"
        )));
    }

    let access = local_access(std::env::var(READ_TOKEN_ENV).ok(), browser_origins)?;
    if explorer_repo.is_some() && (!bind.ip().is_loopback() || access.token.is_none()) {
        return Err(Error::Invalid(format!(
            "the repository explorer requires a loopback bind address and {READ_TOKEN_ENV}"
        )));
    }
    #[cfg(unix)]
    let explorer = explorer::open(explorer_repo, explorer_repository, explorer_spool)?;
    #[cfg(not(unix))]
    if explorer_repo.is_some() || explorer_repository.is_some() || explorer_spool.is_some() {
        return Err(Error::Invalid(
            "the repository explorer needs a Unix host".into(),
        ));
    }

    // Validate and initialize the engine before reporting the listening address.
    let engine = Engine::open(&data_dir)?;
    let data_dir = std::fs::canonicalize(data_dir)?;
    let collisions = match collisions.map(std::fs::canonicalize).transpose()? {
        Some(path) if !path.is_dir() => {
            return Err(Error::Invalid(
                "collisions repository must be a directory".into(),
            ));
        }
        path => path.map(Arc::new),
    };
    let timeline = match timeline {
        Some(root) => {
            let root = std::fs::canonicalize(root)?;
            // Fail at startup, as the CLI would, rather than on the first request.
            timeline_value(
                &data_dir,
                &root,
                TimelineRead::Page {
                    limit: 1,
                    before: None,
                },
            )?;
            Some(Arc::new(root))
        }
        None => None,
    };

    #[cfg(unix)]
    let door = front_door
        .then(front_door::FrontDoor::new)
        .transpose()?
        .map(Arc::new);
    let state = AppState {
        data_dir,
        engine,
        work: Arc::new(Semaphore::new(max_concurrency)),
        collisions,
        timeline,
        #[cfg(unix)]
        explorer,
        #[cfg(unix)]
        front_door: door.clone(),
    };
    #[cfg(unix)]
    if let Some(door) = door {
        return serve_front_door(state, door, bind).await;
    }
    let app = router(state, access);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("listening on {}", listener.local_addr()?);
    agentscm::http_listener::serve(listener, app, shutdown_signal()).await;
    Ok(())
}

/// Bind the front door (the requested port, else any free one), publish its discovery record,
/// serve until stopped, then revoke the Hub tokens it minted.
#[cfg(unix)]
async fn serve_front_door(
    state: AppState,
    door: Arc<front_door::FrontDoor>,
    bind: SocketAddr,
) -> Result<()> {
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            tokio::net::TcpListener::bind(SocketAddr::new(bind.ip(), 0)).await?
        }
        Err(error) => return Err(error.into()),
    };
    let address = listener.local_addr()?;
    let base = format!("http://{address}");
    door.set_origin(base.clone());
    let _record = crate::local_service::publish(&crate::local_service::ServiceRecord {
        version: 1,
        instance: door.instance.clone(),
        pid: std::process::id(),
        base,
        admin: door.admin_secret().to_owned(),
        started_at: agentscm::hub::store::unix_now(),
    })?;
    eprintln!("listening on {address}");
    let app = front_door::router(state, door.clone());
    agentscm::http_listener::serve(listener, app, shutdown_signal()).await;
    door.revoke_all().await;
    Ok(())
}

fn local_access(token: Option<String>, origins: Vec<String>) -> Result<LocalAccess> {
    if token
        .as_ref()
        .is_some_and(|token| token.len() < MIN_READ_TOKEN_BYTES)
    {
        return Err(Error::Invalid(format!(
            "{READ_TOKEN_ENV} must contain at least {MIN_READ_TOKEN_BYTES} bytes"
        )));
    }
    if !origins.is_empty() && token.is_none() {
        return Err(Error::Invalid(format!(
            "--browser-origin requires {READ_TOKEN_ENV}"
        )));
    }
    let origins = origins
        .into_iter()
        .map(|origin| {
            let host = origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"))
                .filter(|host| !host.is_empty() && !host.contains('/'));
            host.and_then(|_| HeaderValue::from_str(&origin).ok())
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "browser origin must be scheme://host[:port], got {origin:?}"
                    ))
                })
        })
        .collect::<Result<_>>()?;
    Ok(LocalAccess { token, origins })
}

fn router(state: AppState, access: LocalAccess) -> Router {
    let local = Router::new()
        .route("/v1/local/skill-workflow-library", get(library_inspect))
        .route("/v1/local/skill-workflows", get(skill_workflow_page))
        .route("/v1/local/skill-workflows/{run_id}/handoff", get(handoff))
        .route("/v1/local/changes", get(change_page))
        .route("/v1/local/workspace/timeline", get(timeline_page))
        .route("/v1/local/workspace/timeline/{id}", get(timeline_entry))
        .route("/v1/local/workspace/diff", get(timeline_diff));
    #[cfg(unix)]
    let local = local.merge(explorer::routes());
    let local = local
        .layer(middleware::from_fn_with_state(
            Arc::new(access),
            local_guard,
        ))
        .with_state(state.clone());
    Router::new()
        .route("/health", get(health))
        .route("/v1/revisions/{revision}", get(revision))
        .route("/v1/revisions/{revision}/file", get(read_file))
        .route("/v1/diff", get(diff))
        .route("/v1/workflow", get(workflow_status))
        .route("/v1/collisions", get(collisions_status))
        .route("/v1/workflow/inspection", get(workflow_inspection))
        .route("/v1/skill-workflows/{run_id}", get(skill_workflow))
        .with_state(state)
        .merge(local)
}

/// Answers CORS preflight for configured origins and checks the read token before any
/// `/v1/local` handler runs. Only GET handlers are routed, so other methods get 405.
async fn local_guard(
    State(access): State<Arc<LocalAccess>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .filter(|origin| access.origins.contains(origin))
        .cloned();
    let mut response = if !loopback_host(request.headers()) {
        // A rebound DNS name reaches this loopback listener with its own Host.
        read_error(
            ApiError {
                status: StatusCode::MISDIRECTED_REQUEST,
                message: "local reads require a loopback Host (127.0.0.1, localhost or [::1])"
                    .into(),
            },
            "local",
        )
    } else if request.method() == Method::OPTIONS {
        if origin.is_some() {
            StatusCode::NO_CONTENT.into_response()
        } else {
            read_error(
                ApiError {
                    status: StatusCode::FORBIDDEN,
                    message: "origin is not allowed; start serve with --browser-origin".into(),
                },
                "local",
            )
        }
    } else if request.uri().path().starts_with("/v1/local/explorer/")
        && request.headers().contains_key(header::ORIGIN)
        && origin.is_none()
    {
        read_error(
            ApiError {
                status: StatusCode::FORBIDDEN,
                message: "origin is not allowed; start serve with --browser-origin".into(),
            },
            "repository explorer",
        )
    } else if !authorized(access.token.as_deref(), request.headers()) {
        let mut response = read_error(
            ApiError {
                status: StatusCode::UNAUTHORIZED,
                message: format!("a valid bearer token from {READ_TOKEN_ENV} is required"),
            },
            "local",
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        response
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
    }
    response
}

pub(super) fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
    else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => name,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

fn authorized(token: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(token) = token else {
        return true;
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .as_bytes();
    // Constant time for equal lengths, as agentscm's hmac comparison.
    presented.len() == token.len()
        && presented
            .iter()
            .zip(token.as_bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Pages recent workflow runs, optionally filtered by `q`, as the Hub does.
async fn skill_workflow_page(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let parsed = named_fields(raw.as_deref(), &["before", "q"]).and_then(|fields| {
        let before: Option<i64> = optional_number(&fields, "before")?;
        let search = agentscm::hub::store::search_text(fields.get("q").map(String::as_str))
            .map_err(|_| {
                ApiError::limit("q must be at most 200 bytes without control characters")
            })?;
        Ok((before, search))
    });
    let (before, search) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => return read_error(error, "skill workflow list"),
    };
    let data_dir = state.data_dir.clone();
    local_read(state, "skill workflow list", move || {
        let page =
            agentscm::skill_workflow_service::list_page(&data_dir, 16, before, search.as_deref())?;
        Ok(serde_json::to_value(page)?)
    })
    .await
}

/// Pages current Change pointers by ID: `limit` (1..=100, default 50) and the last ID as `after`.
async fn change_page(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let parsed = named_fields(raw.as_deref(), &["limit", "after"]).and_then(|mut fields| {
        let limit: usize = optional_number(&fields, "limit")?.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(ApiError::limit("limit must be 1 through 100"));
        }
        let after = fields.remove("after");
        if let Some(after) = &after {
            check_text(after, "after")?;
        }
        Ok((limit, after))
    });
    let (limit, after) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => return read_error(error, "change list"),
    };
    let data_dir = state.data_dir.clone();
    local_read(state, "change list", move || {
        // One extra row says whether more follow; at the 100-row cap a full page counts as more.
        let mut changes = agentscm::authority::Authority::open_read_only(&data_dir)?
            .list_changes((limit + 1).min(100), after.as_deref())?;
        let more = changes.len() > limit || (limit == 100 && changes.len() == 100);
        changes.truncate(limit);
        let next_after = more.then(|| changes.last().map(|c| c.id.id.clone()));
        let changes: Vec<_> = changes
            .iter()
            .map(|c| json!({"id": c.id.id, "version": c.version}))
            .collect();
        Ok(json!({"changes": changes, "next_after": next_after.flatten()}))
    })
    .await
}

async fn library_inspect(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let (change, library) = match named_fields(raw.as_deref(), &["change", "library"]) {
        Ok(mut fields) => match (fields.remove("change"), fields.remove("library")) {
            (Some(change), Some(library)) => (change, library),
            _ => {
                return read_error(
                    ApiError::limit("query requires change and library"),
                    "workflow library",
                );
            }
        },
        Err(error) => return read_error(error, "workflow library"),
    };
    if let Err(error) = check_text(&change, "change").and(check_text(&library, "library")) {
        return read_error(error, "workflow library");
    }
    let data_dir = state.data_dir.clone();
    local_read(state, "workflow library", move || {
        crate::skill_workflow_cli::library_value(
            &data_dir,
            crate::skill_workflow_cli::LibraryCommand::Inspect { change, library },
        )
    })
    .await
}

async fn handoff(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Err(error) = named_fields(raw.as_deref(), &[]) {
        return read_error(error, "workflow handoff");
    }
    if let Err(error) = agentscm::skill_workflow_service::validate_run_id(&run_id) {
        return read_error(ApiError::from(error), "workflow handoff");
    }
    let data_dir = state.data_dir.clone();
    local_read(state, "workflow handoff", move || {
        crate::skill_workflow_cli::handoff_value(&data_dir, &run_id)
    })
    .await
}

enum TimelineRead {
    Page {
        limit: usize,
        before: Option<u64>,
    },
    Entry(u64),
    Diff {
        left: u64,
        right: u64,
        limit: usize,
        after: Option<String>,
    },
}

#[cfg(unix)]
fn timeline_value(
    data_dir: &std::path::Path,
    root: &std::path::Path,
    read: TimelineRead,
) -> Result<serde_json::Value> {
    // A fresh service per read revalidates the pinned paths, as each CLI invocation does.
    let service =
        agentscm::workspace_timeline_service::WorkspaceTimelineService::new(data_dir, root, None)?;
    match read {
        TimelineRead::Page { limit, before } => service.timeline(limit, before),
        TimelineRead::Entry(id) => service.entry(id),
        TimelineRead::Diff {
            left,
            right,
            limit,
            after,
        } => service.diff(left, right, limit, after),
    }
}

#[cfg(not(unix))]
fn timeline_value(
    _: &std::path::Path,
    _: &std::path::Path,
    _: TimelineRead,
) -> Result<serde_json::Value> {
    Err(Error::Invalid(
        "workspace timelines need a Unix host".into(),
    ))
}

async fn timeline_page(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let read = named_fields(raw.as_deref(), &["limit", "before"]).and_then(|fields| {
        Ok(TimelineRead::Page {
            limit: optional_number(&fields, "limit")?.unwrap_or(25),
            before: optional_number(&fields, "before")?,
        })
    });
    timeline_read(state, read).await
}

async fn timeline_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let read = named_fields(raw.as_deref(), &[]).and_then(|_| {
        id.parse()
            .map(TimelineRead::Entry)
            .map_err(|_| ApiError::limit("timeline entry id must be an integer"))
    });
    timeline_read(state, read).await
}

async fn timeline_diff(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let read = named_fields(raw.as_deref(), &["left", "right", "limit", "after"]).and_then(
        |mut fields| {
            let required = |name| {
                optional_number(&fields, name)?
                    .ok_or_else(|| ApiError::limit(format!("query is missing {name}")))
            };
            let (left, right) = (required("left")?, required("right")?);
            let limit = optional_number(&fields, "limit")?.unwrap_or(100);
            let after = fields.remove("after");
            if let Some(after) = &after {
                check_text(after, "after")?;
            }
            Ok(TimelineRead::Diff {
                left,
                right,
                limit,
                after,
            })
        },
    );
    timeline_read(state, read).await
}

async fn timeline_read(
    state: AppState,
    read: std::result::Result<TimelineRead, ApiError>,
) -> Response {
    // The workspace comes from the serve flag, never from an HTTP parameter.
    let Some(root) = state.timeline.clone() else {
        let error = ApiError {
            status: StatusCode::NOT_FOUND,
            message: "workspace timeline is off; start serve with --timeline-workspace".into(),
        };
        return read_error(error, "workspace timeline");
    };
    let read = match read {
        Ok(read) => read,
        Err(error) => return read_error(error, "workspace timeline"),
    };
    let data_dir = state.data_dir.clone();
    local_read(state, "workspace timeline", move || {
        timeline_value(&data_dir, &root, read)
    })
    .await
}

/// Runs one CLI read in a work slot and returns its JSON unchanged, without the CLI envelope.
async fn local_read<F>(state: AppState, what: &'static str, read: F) -> Response
where
    F: FnOnce() -> Result<serde_json::Value> + Send + 'static,
{
    let result = blocking(state, move |_| {
        let body = serde_json::to_string(&read()?)?;
        if body.len() > MAX_SKILL_WORKFLOW_BYTES {
            return Err(Error::Limit(format!(
                "{what} response exceeds {MAX_SKILL_WORKFLOW_BYTES} bytes"
            )));
        }
        Ok(body)
    })
    .await;
    match result {
        Ok((body, permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Err(error) => read_error(error, what),
    }
}

/// Parses an optional query whose fields must be distinct names from `allowed`.
fn named_fields(
    raw: Option<&str>,
    allowed: &[&str],
) -> std::result::Result<std::collections::BTreeMap<String, String>, ApiError> {
    let mut fields = std::collections::BTreeMap::new();
    if raw.is_none_or(str::is_empty) {
        return Ok(fields);
    }
    for (name, value) in query_fields(raw)? {
        if !allowed.contains(&name.as_str()) || fields.contains_key(&name) {
            return Err(ApiError::limit("query has an unknown or duplicate field"));
        }
        fields.insert(name, value);
    }
    Ok(fields)
}

fn optional_number<T: std::str::FromStr>(
    fields: &std::collections::BTreeMap<String, String>,
    name: &str,
) -> std::result::Result<Option<T>, ApiError> {
    fields
        .get(name)
        .map(|value| {
            value
                .parse()
                .map_err(|_| ApiError::limit(format!("{name} must be a non-negative integer")))
        })
        .transpose()
}

async fn skill_workflow(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return read_error(
            ApiError::limit("skill workflow reads accept no query fields"),
            "skill workflow",
        );
    }
    if let Err(error) = agentscm::skill_workflow_service::validate_run_id(&run_id) {
        return read_error(ApiError::from(error), "skill workflow");
    }

    let data_dir = state.data_dir.clone();
    let permit = match reserve_work(&state) {
        Ok(permit) => permit,
        Err(error) => return read_error(error, "skill workflow"),
    };
    let result = blocking_reserved(state, permit, move |engine| {
        let projected = (|| {
            let view = agentscm::skill_workflow_service::read(&data_dir, &engine, &run_id)?;
            let body = serde_json::to_string(&view)?;
            if body.len() > MAX_SKILL_WORKFLOW_BYTES {
                Err(Error::Limit(format!(
                    "skill workflow response exceeds {MAX_SKILL_WORKFLOW_BYTES} bytes"
                )))
            } else {
                Ok(body)
            }
        })();
        projected.map_err(|error| match error {
            Error::Invalid(message) | Error::Limit(message) => Error::Io(std::io::Error::other(
                format!("skill workflow projection validation failed: {message}"),
            )),
            error => error,
        })
    })
    .await;
    match result {
        Ok((body, permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Err(error) => read_error(error, "skill workflow"),
    }
}

async fn workflow_status(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return read_error(
            ApiError::limit("workflow status accepts no query fields"),
            "workflow",
        );
    }
    workflow_read(state, agentscm::guided_service::ReadRequest::Status).await
}

async fn workflow_inspection(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let limit = match workflow_limit(raw.as_deref()) {
        Ok(limit) => limit.unwrap_or(4096),
        Err(error) => return read_error(error, "workflow"),
    };
    workflow_read(
        state,
        agentscm::guided_service::ReadRequest::Inspect { limit },
    )
    .await
}

fn workflow_limit(raw: Option<&str>) -> std::result::Result<Option<usize>, ApiError> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let fields = query_fields(Some(raw))?;
    if fields.len() != 1 || fields[0].0 != "limit" {
        return Err(ApiError::limit(
            "workflow inspection accepts only one limit field",
        ));
    }
    let limit: usize = fields[0]
        .1
        .parse()
        .map_err(|_| ApiError::limit("workflow inspection limit must be an integer"))?;
    if !(1..=4096).contains(&limit) {
        return Err(ApiError::limit(
            "workflow inspection limit must be between 1 and 4096",
        ));
    }
    Ok(Some(limit))
}

async fn workflow_read(
    state: AppState,
    request: agentscm::guided_service::ReadRequest,
) -> Response {
    // This path is chosen by the operator at startup, never by an HTTP parameter.
    let data_dir = state.data_dir.clone();
    let result = blocking(state, move |engine| {
        let projected = agentscm::guided_service::read(&data_dir, &engine, request)
            .and_then(|view| Ok(serde_json::to_string(&view)?));
        projected.map_err(|error| match error {
            Error::Invalid(message) => Error::Io(std::io::Error::other(format!(
                "workflow projection validation failed: {message}"
            ))),
            error => error,
        })
    })
    .await;
    match result {
        Ok((body, permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Err(error) => read_error(error, "workflow"),
    }
}

#[cfg(unix)]
fn collisions_report(repository: &std::path::Path) -> Result<serde_json::Value> {
    agentscm::git_collisions::collisions(&agentscm::git_collisions::Request {
        git: PathBuf::from("/usr/bin/git"),
        repository: repository.to_path_buf(),
        base: None,
        refs: vec![],
        worktrees: true,
        active_days: Some(14),
        include_merged: false,
        deadline: Some(std::time::Instant::now() + COLLISIONS_DEADLINE),
        // Headroom for the base, version and notes outside the budgeted lists.
        max_bytes: Some(MAX_COLLISIONS_BYTES - 16 * 1024),
    })
}

#[cfg(not(unix))]
fn collisions_report(_: &std::path::Path) -> Result<serde_json::Value> {
    Err(Error::Invalid("collision checks need a Unix host".into()))
}

fn read_error(error: ApiError, what: &str) -> Response {
    let message = if error.status == StatusCode::INTERNAL_SERVER_ERROR {
        let _ = writeln!(
            std::io::stderr().lock(),
            "{what} read failed: {:?}",
            error.message
        );
        format!("The {what} read failed. Inspect local service diagnostics before retrying.")
    } else {
        error.message
    };
    api_response(
        error.status,
        json!({"error":message}).to_string(),
        "application/json",
    )
}

async fn collisions_status(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    collisions_read(state, raw).await
}

async fn collisions_read(state: AppState, raw: Option<String>) -> Response {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return read_error(
            ApiError::limit("collisions accept no query fields"),
            "collisions",
        );
    }
    // The repository comes from the serve flag, never from an HTTP parameter.
    let Some(repository) = state.collisions.clone() else {
        let error = ApiError {
            status: StatusCode::NOT_FOUND,
            message: "collisions are off; start serve with --collisions-repo".into(),
        };
        return read_error(error, "collisions");
    };
    // The scan uses the shared work slot, so --max-concurrency still bounds memory.
    let result = blocking(state, move |_| {
        // Its own messages suggest CLI flags; over HTTP the operator fixes the checkout.
        let report = match collisions_report(&repository) {
            Ok(report) => report,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let json = report.to_string();
        if json.len() > MAX_COLLISIONS_BYTES {
            return Ok(Err(format!(
                "report of {} bytes exceeds {MAX_COLLISIONS_BYTES}",
                json.len()
            )));
        }
        Ok(Ok(json))
    })
    .await;
    match result {
        Ok((Ok(body), permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Ok((Err(detail), _)) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "collisions check failed: {detail}"
            );
            let error = ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "The collision check could not run or its report was too large. The configured checkout needs origin/HEAD or main, at most 200 in-flight branches and worktrees, and Git 2.43 or newer at /usr/bin/git; for larger reports use agentscm collisions. The service log has the detail.".into(),
            };
            read_error(error, "collisions")
        }
        Err(error) => read_error(error, "collisions"),
    }
}

fn api_response(status: StatusCode, body: String, content_type: &'static str) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    response
}

pub(super) async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn revision(
    State(state): State<AppState>,
    Path(revision_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    check_text(&revision_id, "revision")?;
    let id = revision_id.clone();
    let (revision, permit) =
        blocking(state, move |engine| engine.load_revision(&revision_id)).await?;
    Ok(guard_response(
        Json(RevisionResponse { id, revision }).into_response(),
        permit,
    ))
}

async fn read_file(
    State(state): State<AppState>,
    Path(revision): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> std::result::Result<Response, ApiError> {
    let query = read_query(raw_query.as_deref())?;
    check_text(&revision, "revision")?;
    check_text(&query.path, "path")?;
    let ((kind, content), permit) = blocking(state, move |engine| {
        engine.read_entry(&revision, &query.path)
    })
    .await?;
    if content.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::limit("source response exceeds 8388608 bytes"));
    }
    let mut response = Response::new(Body::from(content));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        "x-agentscm-entry-kind",
        HeaderValue::from_static(kind.as_str()),
    );
    Ok(guard_response(response, permit))
}

async fn diff(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> std::result::Result<Response, ApiError> {
    let query = diff_query(raw_query.as_deref())?;
    check_text(&query.left, "left")?;
    check_text(&query.right, "right")?;
    if query.limit == 0 || query.limit > MAX_DIFF_LIMIT {
        return Err(ApiError::limit("diff limit must be between 1 and 10000"));
    }
    let (result, permit) = blocking(state, move |engine| {
        engine.diff(&query.left, &query.right, query.limit)
    })
    .await?;
    Ok(guard_response(Json(result).into_response(), permit))
}

async fn blocking<T, F>(
    state: AppState,
    operation: F,
) -> std::result::Result<(T, OwnedSemaphorePermit), ApiError>
where
    T: Send + 'static,
    F: FnOnce(Engine) -> Result<T> + Send + 'static,
{
    let permit = reserve_work(&state)?;
    blocking_reserved(state, permit, operation).await
}

fn reserve_work(state: &AppState) -> std::result::Result<OwnedSemaphorePermit, ApiError> {
    state
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "server is at its concurrent work limit".to_owned(),
        })
}

async fn blocking_reserved<T, F>(
    state: AppState,
    permit: OwnedSemaphorePermit,
    operation: F,
) -> std::result::Result<(T, OwnedSemaphorePermit), ApiError>
where
    T: Send + 'static,
    F: FnOnce(Engine) -> Result<T> + Send + 'static,
{
    let engine = state.engine.clone();
    tokio::task::spawn_blocking(move || run_blocking(engine, permit, operation))
        .await
        .map_err(|_| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "blocking operation failed".to_owned(),
        })?
        .map_err(ApiError::from)
}

fn run_blocking<T, F>(
    engine: Engine,
    permit: OwnedSemaphorePermit,
    operation: F,
) -> Result<(T, OwnedSemaphorePermit)>
where
    F: FnOnce(Engine) -> Result<T>,
{
    operation(engine).map(|result| (result, permit))
}

fn guard_response(response: Response, permit: OwnedSemaphorePermit) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(ResponseGuard {
            inner: body,
            pending: None,
            _permit: permit,
        }),
    )
}

impl HttpBody for ResponseGuard {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
        loop {
            if let Some(pending) = self.pending.as_mut() {
                if !pending.is_empty() {
                    let chunk_len = pending.len().min(RESPONSE_CHUNK_BYTES);
                    let chunk = Bytes::copy_from_slice(&pending[..chunk_len]);
                    let _ = pending.split_to(chunk_len);
                    return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
                }
                self.pending = None;
            }

            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.is_empty() => continue,
                    Ok(data) => self.pending = Some(data),
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.pending.as_ref().is_none_or(Bytes::is_empty) && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let inner = self.inner.size_hint();
        let pending = self.pending.as_ref().map_or(0, |data| data.len() as u64);
        let mut hint = http_body::SizeHint::new();
        hint.set_lower(pending.saturating_add(inner.lower()));
        if let Some(upper) = inner.upper().and_then(|value| pending.checked_add(value)) {
            hint.set_upper(upper);
        }
        hint
    }
}

fn check_text(value: &str, field: &str) -> std::result::Result<(), ApiError> {
    if value.is_empty() || value.len() > MAX_QUERY_BYTES {
        return Err(ApiError::limit(format!(
            "{field} must contain between 1 and {MAX_QUERY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn read_query(raw: Option<&str>) -> std::result::Result<ReadQuery, ApiError> {
    let fields = query_fields(raw)?;
    if fields.len() != 1 || fields[0].0 != "path" {
        return Err(ApiError::limit("query must contain exactly one path field"));
    }
    Ok(ReadQuery {
        path: fields[0].1.clone(),
    })
}

fn diff_query(raw: Option<&str>) -> std::result::Result<DiffQuery, ApiError> {
    let fields = query_fields(raw)?;
    let mut left = None;
    let mut right = None;
    let mut limit = None;
    for (name, value) in fields {
        match name.as_str() {
            "left" if left.is_none() => left = Some(value),
            "right" if right.is_none() => right = Some(value),
            "limit" if limit.is_none() => {
                limit = Some(
                    value
                        .parse()
                        .map_err(|_| ApiError::limit("diff limit must be an integer"))?,
                );
            }
            _ => return Err(ApiError::limit("query has an unknown or duplicate field")),
        }
    }
    Ok(DiffQuery {
        left: left.ok_or_else(|| ApiError::limit("query is missing left"))?,
        right: right.ok_or_else(|| ApiError::limit("query is missing right"))?,
        limit: limit.unwrap_or(1_000),
    })
}

fn query_fields(raw: Option<&str>) -> std::result::Result<Vec<(String, String)>, ApiError> {
    let raw = raw.ok_or_else(|| ApiError::limit("query is required"))?;
    if raw.len() > MAX_QUERY_BYTES {
        return Err(ApiError::limit(format!(
            "query exceeds {MAX_QUERY_BYTES} bytes"
        )));
    }
    raw.split('&')
        .map(|field| {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| ApiError::limit("query field is missing ="))?;
            Ok((decode_query(name)?, decode_query(value)?))
        })
        .collect()
}

fn decode_query(input: &str) -> std::result::Result<String, ApiError> {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' if index + 2 < input.len() => {
                let high = hex(input[index + 1])?;
                let low = hex(input[index + 2])?;
                bytes.push((high << 4) | low);
                index += 3;
            }
            b'%' => return Err(ApiError::limit("query has incomplete percent encoding")),
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| ApiError::limit("query is not valid UTF-8"))
}

fn hex(byte: u8) -> std::result::Result<u8, ApiError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ApiError::limit("query has invalid percent encoding")),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

impl ApiError {
    fn limit(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match &error {
            Error::Invalid(_) | Error::UnsupportedGitVersion { .. } | Error::Limit(_) => {
                StatusCode::BAD_REQUEST
            }
            Error::NotFound(_) | Error::MissingObject(_) => StatusCode::NOT_FOUND,
            Error::SourceDenied => StatusCode::FORBIDDEN,
            Error::SourceIndeterminate => StatusCode::SERVICE_UNAVAILABLE,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Io(_) | Error::Json(_) | Error::Sql(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn workflow_reads_share_the_existing_work_limit() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState {
            data_dir: directory.path().to_path_buf(),
            engine: Engine::open(directory.path()).unwrap(),
            work: Arc::new(Semaphore::new(1)),
            collisions: None,
            timeline: None,
            #[cfg(unix)]
            explorer: None,
            #[cfg(unix)]
            front_door: None,
        };
        let permit = reserve_work(&state).unwrap();
        let response = workflow_status(State(state.clone()), RawQuery(None)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(state.work.available_permits(), 0);
        drop(permit);
        assert_eq!(state.work.available_permits(), 1);
        assert!(!directory.path().join("workflow.sqlite").exists());
    }

    #[tokio::test]
    async fn skill_workflow_reads_preserve_the_existing_work_limit_response() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState {
            data_dir: directory.path().to_path_buf(),
            engine: Engine::open(directory.path()).unwrap(),
            work: Arc::new(Semaphore::new(1)),
            collisions: None,
            timeline: None,
            #[cfg(unix)]
            explorer: None,
            #[cfg(unix)]
            front_door: None,
        };
        let permit = reserve_work(&state).unwrap();
        let response = skill_workflow(
            State(state.clone()),
            Path("retained-run".into()),
            RawQuery(None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(state.work.available_permits(), 0);
        drop(permit);
        assert_eq!(state.work.available_permits(), 1);
        assert!(!directory.path().join("skill-workflows").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_request_keeps_permit_until_blocking_work_finishes() {
        let data_dir = tempfile::tempdir().unwrap();
        let work = Arc::new(Semaphore::new(1));
        let state = AppState {
            data_dir: data_dir.path().to_path_buf(),
            engine: Engine::open(data_dir.path()).unwrap(),
            work: Arc::clone(&work),
            collisions: None,
            timeline: None,
            #[cfg(unix)]
            explorer: None,
            #[cfg(unix)]
            front_door: None,
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let task = tokio::spawn(blocking(state, move |_engine| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));

        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(work.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if work.clone().try_acquire_owned().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "permit was not released when blocking work finished"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
