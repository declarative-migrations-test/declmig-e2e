mod flags;

use anyhow::{Context as _, Result, anyhow, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

const DEFAULT_ADDR: &str = "127.0.0.1:8765";
const DEFAULT_INGRESS_URL: &str = "http://127.0.0.1:8091";
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_INGRESS_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_INGRESS_RESPONSE_HEADER_VALUES: usize = 32;
const MAX_INGRESS_RESPONSE_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MAX_INGRESS_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
const INGRESS_RESPONSE_HEADER_ALLOWLIST: [&str; 13] = [
    "cache-control",
    "content-type",
    "ratelimit",
    "ratelimit-limit",
    "ratelimit-policy",
    "ratelimit-remaining",
    "ratelimit-reset",
    "retry-after",
    "traceparent",
    "x-ores-rate-limit-decision",
    "x-ores-rate-limit-layer",
    "x-ores-rate-limit-policy",
    "x-request-id",
];
const MAX_MANAGED_PROCESSES: usize = 64;
const MAX_PROCESS_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 16 * 1024;
const MAX_ENV_VARS: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
const MAX_TOKEN_FILE_BYTES: u64 = 4096;
const IDEMPOTENCY_HEADER: &str = "x-ores-idempotency-key";
const MIN_IDEMPOTENCY_KEY_BYTES: usize = 8;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_RECENT_IDEMPOTENCY_KEYS: usize = 4096;
const IDEMPOTENCY_REPLAY_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_REPLAY_FILE_BYTES: u64 = 2 * 1024 * 1024;
const LEGACY_REPLAY_FILE_VERSION: u32 = 1;
const REPLAY_FILE_VERSION: u32 = 2;

#[derive(Clone)]
struct AppState {
    token: Arc<str>,
    replay: Arc<Mutex<ReplayState>>,
    replay_path: Arc<PathBuf>,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    client: reqwest::Client,
    ingress_url: Arc<str>,
    ingress_token: Option<Arc<str>>,
    cloudflared_command: Arc<str>,
    cloudflared_args: Arc<Vec<String>>,
    update_command: Option<Arc<str>>,
    update_args: Arc<Vec<String>>,
    allow_arbitrary_client_commands: bool,
    started_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProcessSpec {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

struct ManagedProcess {
    spec: ProcessSpec,
    child: Child,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayEntry {
    key: String,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Default)]
struct ReplayState {
    order: VecDeque<ReplayEntry>,
    keys: HashSet<String>,
    last_observed_unix_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayFile {
    version: u32,
    entries: Vec<ReplayEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyReplayFile {
    version: u32,
    keys: Vec<String>,
}

impl ReplayState {
    fn reserve(&mut self, key: String, now_unix_ms: u64) -> Result<()> {
        let now_unix_ms = self.observe_time(now_unix_ms);
        self.purge_expired(now_unix_ms);

        if self.keys.contains(&key) {
            bail!("idempotency key was already used");
        }
        if self.order.len() >= MAX_RECENT_IDEMPOTENCY_KEYS {
            bail!("idempotency replay capacity reached; refusing to evict a live reservation");
        }

        let expires_at_unix_ms = now_unix_ms
            .checked_add(IDEMPOTENCY_REPLAY_TTL_MS)
            .ok_or_else(|| anyhow!("idempotency replay expiration overflow"))?;
        self.keys.insert(key.clone());
        self.order.push_back(ReplayEntry {
            key,
            expires_at_unix_ms,
        });
        return Ok(());
    }

    fn release(&mut self, key: &str) -> bool {
        if !self.keys.remove(key) {
            return false;
        }
        self.order.retain(|entry| entry.key != key);
        return true;
    }

    fn observe_time(&mut self, now_unix_ms: u64) -> u64 {
        self.last_observed_unix_ms = self.last_observed_unix_ms.max(now_unix_ms);
        return self.last_observed_unix_ms;
    }

    fn purge_expired(&mut self, now_unix_ms: u64) {
        while self
            .order
            .front()
            .is_some_and(|entry| entry.expires_at_unix_ms <= now_unix_ms)
        {
            if let Some(expired) = self.order.pop_front() {
                self.keys.remove(&expired.key);
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ProcessView {
    name: String,
    command: String,
    running: bool,
    pid: Option<u32>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    runtime: &'static str,
    worker_reuse: &'static str,
    config_source: &'static str,
    arbitrary_client_commands: bool,
    uptime_ms: u128,
    ingress_url: String,
    processes: Vec<ProcessView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeepAwakeRequest {
    enabled: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = flags::apply_cli_flags().map_err(anyhow::Error::msg)?;
    let log_filter = config
        .get("RUST_LOG")
        .map(String::as_str)
        .unwrap_or("scintilla_desktop_daemon=info");
    tracing_subscriber::fmt().with_env_filter(log_filter).init();

    let addr = parse_loopback_addr(
        config
            .get("SCINTILLA_DESKTOP_ADDR")
            .map(String::as_str)
            .unwrap_or(DEFAULT_ADDR),
    )?;
    let token_file = token_path(&config)?;
    let token = load_or_create_token(&token_file)?;
    let replay_path = token_file
        .parent()
        .ok_or_else(|| anyhow!("token path has no parent"))?
        .join("mutation-replay.json");
    let replay = load_replay_state(&replay_path)?;
    let ingress_url = config
        .get("SCINTILLA_LOCAL_INGRESS_URL")
        .cloned()
        .unwrap_or_else(|| DEFAULT_INGRESS_URL.to_owned());
    require_loopback_url(&ingress_url)?;
    let ingress_token = config
        .get("SCINTILLA_LOCAL_INGRESS_TOKEN")
        .filter(|value| !value.is_empty())
        .cloned()
        .map(Arc::<str>::from);

    let cloudflared_command = config
        .get("SCINTILLA_CLOUDFLARED_COMMAND")
        .cloned()
        .unwrap_or_else(|| "cloudflared".to_owned());
    let cloudflared_args = parse_args_json(&config, "SCINTILLA_CLOUDFLARED_ARGS_JSON")?;
    validate_command_vector(&cloudflared_command, &cloudflared_args)?;

    let update_command = config
        .get("SCINTILLA_UPDATE_COMMAND")
        .filter(|value| !value.trim().is_empty())
        .cloned();
    let update_args = parse_args_json(&config, "SCINTILLA_UPDATE_ARGS_JSON")?;
    if let Some(command) = &update_command {
        validate_command_vector(command, &update_args)?;
    }

    let state = AppState {
        token: Arc::from(token),
        replay: Arc::new(Mutex::new(replay)),
        replay_path: Arc::new(replay_path),
        processes: Arc::new(Mutex::new(HashMap::new())),
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        ingress_url: Arc::from(ingress_url),
        ingress_token,
        cloudflared_command: Arc::from(cloudflared_command),
        cloudflared_args: Arc::new(cloudflared_args),
        update_command: update_command.map(Arc::<str>::from),
        update_args: Arc::new(update_args),
        allow_arbitrary_client_commands: config_bool(
            &config,
            "SCINTILLA_ALLOW_ARBITRARY_CLIENT_COMMANDS",
            false,
        )?,
        started_at: Instant::now(),
    };

    if config_bool(&config, "SCINTILLA_START_ERLANG_INGRESS", false)? {
        let command = config
            .get("SCINTILLA_ERLANG_COMMAND")
            .cloned()
            .unwrap_or_else(|| "erl".to_owned());
        let args = parse_args_json(&config, "SCINTILLA_ERLANG_ARGS_JSON")?;
        let spec = ProcessSpec {
            name: "erlang-ingress".to_owned(),
            command,
            args,
            env: BTreeMap::new(),
        };
        start_named(&state, spec).await?;
    }

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/status", get(status))
        .route("/v1/processes", get(processes))
        .route("/v1/processes/start", post(start_process))
        .route("/v1/processes/{name}/stop", post(stop_process))
        .route("/v1/processes/{name}/restart", post(restart_process))
        .route("/v1/tunnel/start", post(start_tunnel))
        .route("/v1/tunnel/stop", post(stop_tunnel))
        .route("/v1/power/keep-awake", post(set_keep_awake))
        .route("/v1/updates/apply", post(apply_update))
        .route("/v1/invoke", post(invoke))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "scintilla desktop daemon listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    stop_all(&state).await;
    return Ok(());
}

async fn health() -> &'static str {
    return "ok";
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    let views = process_views(&state).await.map_err(internal_error)?;
    return Ok(Json(StatusResponse {
        runtime: "scintilla",
        worker_reuse: "runtime_defined",
        config_source: "flags-2-env",
        arbitrary_client_commands: state.allow_arbitrary_client_commands,
        uptime_ms: state.started_at.elapsed().as_millis(),
        ingress_url: state.ingress_url.to_string(),
        processes: views,
    }));
}

async fn processes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProcessView>>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    return process_views(&state)
        .await
        .map(Json)
        .map_err(internal_error);
}

async fn start_process(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(spec): Json<ProcessSpec>,
) -> Result<Json<ProcessView>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    require_client_process_start(state.allow_arbitrary_client_commands)?;
    validate_process_spec(&spec).map_err(bad_request)?;
    reserve_mutation(&headers, &state).await?;
    start_named(&state, spec.clone())
        .await
        .map_err(conflict_or_internal)?;
    return Ok(Json(ProcessView {
        name: spec.name.clone(),
        command: spec.command,
        running: true,
        pid: pid_for(&state, &spec.name).await,
    }));
}

async fn stop_process(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    validate_name(&name)?;
    reserve_mutation(&headers, &state).await?;
    stop_named(&state, &name).await.map_err(internal_error)?;
    return Ok(StatusCode::NO_CONTENT);
}

async fn restart_process(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    validate_name(&name)?;
    reserve_mutation(&headers, &state).await?;
    let spec = {
        let mut guard = state.processes.lock().await;
        let Some(mut managed) = guard.remove(&name) else {
            return Err((StatusCode::NOT_FOUND, "process not found".to_owned()));
        };
        let spec = managed.spec.clone();
        let _ = managed.child.kill().await;
        let _ = managed.child.wait().await;
        spec
    };
    start_named(&state, spec).await.map_err(internal_error)?;
    return Ok(StatusCode::NO_CONTENT);
}

async fn start_tunnel(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    reserve_mutation(&headers, &state).await?;
    let spec = ProcessSpec {
        name: "cloudflared".to_owned(),
        command: state.cloudflared_command.to_string(),
        args: state.cloudflared_args.as_ref().clone(),
        env: BTreeMap::new(),
    };
    start_named(&state, spec)
        .await
        .map_err(conflict_or_internal)?;
    return Ok(StatusCode::ACCEPTED);
}

async fn stop_tunnel(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    reserve_mutation(&headers, &state).await?;
    stop_named(&state, "cloudflared")
        .await
        .map_err(internal_error)?;
    return Ok(StatusCode::NO_CONTENT);
}

async fn set_keep_awake(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<KeepAwakeRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    if !request.enabled {
        reserve_mutation(&headers, &state).await?;
        stop_named(&state, "keep-awake")
            .await
            .map_err(internal_error)?;
        return Ok(StatusCode::NO_CONTENT);
    }

    let (command, args) = keep_awake_command().map_err(internal_error)?;
    reserve_mutation(&headers, &state).await?;
    let spec = ProcessSpec {
        name: "keep-awake".to_owned(),
        command,
        args,
        env: BTreeMap::new(),
    };
    start_named(&state, spec)
        .await
        .map_err(conflict_or_internal)?;
    return Ok(StatusCode::ACCEPTED);
}

async fn apply_update(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    authorize(&headers, &state)?;
    let Some(command) = state.update_command.as_deref() else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "SCINTILLA_UPDATE_COMMAND is not configured".to_owned(),
        ));
    };
    reserve_mutation(&headers, &state).await?;
    let status = Command::new(command)
        .args(state.update_args.iter())
        .status()
        .await
        .map_err(internal_error)?;
    if !status.success() {
        return Err((StatusCode::BAD_GATEWAY, "update command failed".to_owned()));
    }
    return Ok(StatusCode::NO_CONTENT);
}

async fn invoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    authorize(&headers, &state)?;
    reserve_mutation(&headers, &state).await?;
    let url = format!("{}/v1/invoke", state.ingress_url.trim_end_matches('/'));
    let mut request = state.client.post(url).json(&payload);
    if let Some(token) = &state.ingress_token {
        request = request.bearer_auth(token.as_ref());
    }
    let upstream = request.send().await.map_err(bad_gateway)?;
    let release_idempotency = is_definitive_quota_rejection(upstream.status(), upstream.headers());
    let response = proxy_ingress_response(upstream, MAX_INGRESS_RESPONSE_BYTES)
        .await
        .map_err(bad_gateway)?;
    if release_idempotency {
        release_mutation(&headers, &state).await?;
    }
    return Ok(response);
}

async fn start_named(state: &AppState, spec: ProcessSpec) -> Result<()> {
    validate_process_spec(&spec)?;
    let mut guard = state.processes.lock().await;
    if guard.contains_key(&spec.name) {
        bail!("process already exists: {}", spec.name);
    }
    if guard.len() >= MAX_MANAGED_PROCESSES {
        bail!("managed process limit reached: {MAX_MANAGED_PROCESSES}");
    }
    let child = spawn(&spec)?;
    guard.insert(spec.name.clone(), ManagedProcess { spec, child });
    return Ok(());
}

fn spawn(spec: &ProcessSpec) -> Result<Child> {
    let mut command = Command::new(&spec.command);
    command.args(&spec.args);
    command.envs(&spec.env);
    command.kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("failed to start {}", spec.name))?;
    return Ok(child);
}

async fn stop_named(state: &AppState, name: &str) -> Result<()> {
    let mut managed = {
        let mut guard = state.processes.lock().await;
        guard.remove(name)
    };
    if let Some(managed) = managed.as_mut() {
        let _ = managed.child.kill().await;
        let _ = managed.child.wait().await;
    }
    return Ok(());
}

async fn stop_all(state: &AppState) {
    let names = {
        let guard = state.processes.lock().await;
        guard.keys().cloned().collect::<Vec<_>>()
    };
    for name in names {
        let _ = stop_named(state, &name).await;
    }
}

async fn process_views(state: &AppState) -> Result<Vec<ProcessView>> {
    let mut guard = state.processes.lock().await;
    let mut views = Vec::with_capacity(guard.len());
    for managed in guard.values_mut() {
        let running = managed.child.try_wait()?.is_none();
        views.push(ProcessView {
            name: managed.spec.name.clone(),
            command: managed.spec.command.clone(),
            running,
            pid: managed.child.id(),
        });
    }
    views.sort_by(|left, right| left.name.cmp(&right.name));
    return Ok(views);
}

async fn pid_for(state: &AppState, name: &str) -> Option<u32> {
    let guard = state.processes.lock().await;
    return guard.get(name).and_then(|managed| managed.child.id());
}

fn authorize(headers: &HeaderMap, state: &AppState) -> Result<(), (StatusCode, String)> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided.is_some_and(|token| constant_time_eq(token.as_bytes(), state.token.as_bytes())) {
        return Ok(());
    }
    return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("{IDEMPOTENCY_HEADER} is required for mutating requests"),
            )
        })?;
    validate_idempotency_key(key).map_err(bad_request)?;
    return Ok(key.to_owned());
}

fn validate_idempotency_key(key: &str) -> Result<()> {
    let bytes = key.as_bytes();
    if bytes.len() < MIN_IDEMPOTENCY_KEY_BYTES || bytes.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        bail!(
            "idempotency key must contain {MIN_IDEMPOTENCY_KEY_BYTES}..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"
        );
    }
    if !bytes.iter().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        bail!("idempotency key contains unsupported characters");
    }
    return Ok(());
}

async fn release_mutation(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<(), (StatusCode, String)> {
    let key = idempotency_key(headers)?;
    let mut replay = state.replay.lock().await;
    let previous = replay.clone();
    if !replay.release(&key) {
        return Ok(());
    }
    if let Err(error) = save_replay_state(&state.replay_path, &replay) {
        *replay = previous;
        return Err(internal_error(error));
    }
    return Ok(());
}

async fn reserve_mutation(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<(), (StatusCode, String)> {
    let key = idempotency_key(headers)?;
    let now_unix_ms = unix_time_ms().map_err(internal_error)?;
    let mut replay = state.replay.lock().await;
    let previous = replay.clone();
    if let Err(error) = replay.reserve(key, now_unix_ms) {
        return Err((StatusCode::CONFLICT, error.to_string()));
    }
    if let Err(error) = save_replay_state(&state.replay_path, &replay) {
        *replay = previous;
        return Err(internal_error(error));
    }
    return Ok(());
}

fn require_client_process_start(allowed: bool) -> Result<(), (StatusCode, String)> {
    if allowed {
        return Ok(());
    }
    return Err((
        StatusCode::FORBIDDEN,
        "arbitrary client process starts are disabled; use declared runtime/tunnel lifecycle endpoints"
            .to_owned(),
    ));
}

fn validate_name(value: &str) -> Result<(), (StatusCode, String)> {
    return validate_name_result(value).map_err(bad_request);
}

fn validate_name_result(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("invalid process name");
    }
    return Ok(());
}

fn validate_process_spec(spec: &ProcessSpec) -> Result<()> {
    validate_name_result(&spec.name)?;
    validate_command_vector(&spec.command, &spec.args)?;
    if spec.env.len() > MAX_ENV_VARS {
        bail!("process environment exceeds {MAX_ENV_VARS} entries");
    }
    for (name, value) in &spec.env {
        if name.is_empty()
            || name.len() > 256
            || name.contains('=')
            || name.chars().any(char::is_control)
        {
            bail!("invalid process environment key");
        }
        if value.len() > MAX_ENV_VALUE_BYTES || value.chars().any(|ch| ch == '\0') {
            bail!("process environment value exceeds desktop limits");
        }
    }
    return Ok(());
}

fn validate_command_vector(command: &str, args: &[String]) -> Result<()> {
    if command.trim().is_empty()
        || command.len() > MAX_ARG_BYTES
        || command.chars().any(char::is_control)
    {
        bail!("command is empty, too long, or contains control characters");
    }
    if args.len() > MAX_PROCESS_ARGS {
        bail!("process argument vector exceeds {MAX_PROCESS_ARGS} entries");
    }
    if args
        .iter()
        .any(|arg| arg.len() > MAX_ARG_BYTES || arg.chars().any(|ch| ch == '\0'))
    {
        bail!("process argument exceeds desktop limits");
    }
    return Ok(());
}

fn parse_loopback_addr(value: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = value
        .parse()
        .context("SCINTILLA_DESKTOP_ADDR is not a socket address")?;
    if !addr.ip().is_loopback() {
        bail!("SCINTILLA_DESKTOP_ADDR must bind to loopback");
    }
    return Ok(addr);
}

fn require_loopback_url(value: &str) -> Result<()> {
    let url =
        reqwest::Url::parse(value).context("SCINTILLA_LOCAL_INGRESS_URL must be a valid URL")?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("SCINTILLA_LOCAL_INGRESS_URL must be credential-free loopback HTTP");
    }
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "::1" | "[::1]") {
        bail!("SCINTILLA_LOCAL_INGRESS_URL must use a literal loopback address");
    }
    if url.port().is_none() {
        bail!("SCINTILLA_LOCAL_INGRESS_URL must include an explicit port");
    }
    if !matches!(url.path(), "" | "/") {
        bail!("SCINTILLA_LOCAL_INGRESS_URL must be a root origin without a path");
    }
    return Ok(());
}

fn parse_args_json(config: &flags::EnvMap, name: &str) -> Result<Vec<String>> {
    let raw = config.get(name).map(String::as_str).unwrap_or("[]");
    let args = serde_json::from_str::<Vec<String>>(raw)
        .with_context(|| format!("{name} must be a JSON string array"))?;
    validate_command_vector("configured-command", &args)?;
    return Ok(args);
}

fn config_bool(config: &flags::EnvMap, name: &str, default_value: bool) -> Result<bool> {
    let Some(value) = config.get(name) else {
        return Ok(default_value);
    };
    return match value.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{name} must be a boolean")),
    };
}

fn token_path(config: &flags::EnvMap) -> Result<PathBuf> {
    if let Some(path) = config
        .get("SCINTILLA_DESKTOP_TOKEN_FILE")
        .filter(|value| !value.trim().is_empty())
    {
        return expand_home(Path::new(path));
    }
    return Ok(home_dir()?.join(".scintilla/daemon/token"));
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(home_dir()?.join(suffix));
    }
    return Ok(path.to_path_buf());
}

fn home_dir() -> Result<PathBuf> {
    return std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME/USERPROFILE is required"));
}

fn read_token_file(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect token file {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("desktop daemon token path must be a regular non-symlink file");
    }
    if metadata.len() == 0 || metadata.len() > MAX_TOKEN_FILE_BYTES {
        bail!("desktop daemon token file size is invalid");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("desktop daemon token file must not be accessible by group/other users");
        }
    }

    let token = fs::read_to_string(path)
        .with_context(|| format!("cannot read desktop daemon token file {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_whitespace) {
        bail!("desktop daemon token file is malformed");
    }
    return Ok(Some(token.to_owned()));
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Some(token) = read_token_file(path)? {
        return Ok(token);
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("token path has no parent"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create token directory {}", parent.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(format!("{token}\n").as_bytes())?;
                file.sync_all()?;
                return Ok(token);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return read_token_file(path)?.ok_or_else(|| {
                    anyhow!("desktop daemon token file appeared but could not be read")
                });
            }
            Err(error) => return Err(error.into()),
        }
    }

    #[cfg(not(unix))]
    {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(format!("{token}\n").as_bytes())?;
                file.sync_all()?;
                return Ok(token);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return read_token_file(path)?.ok_or_else(|| {
                    anyhow!("desktop daemon token file appeared but could not be read")
                });
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn unix_time_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    return u64::try_from(elapsed.as_millis()).context("system clock milliseconds exceed u64");
}

fn replay_state_from_entries(entries: Vec<ReplayEntry>, now_unix_ms: u64) -> Result<ReplayState> {
    if entries.len() > MAX_RECENT_IDEMPOTENCY_KEYS {
        bail!(
            "mutation replay state contains {} entries; maximum is {MAX_RECENT_IDEMPOTENCY_KEYS}",
            entries.len()
        );
    }

    let rearmed_expiry = now_unix_ms
        .checked_add(IDEMPOTENCY_REPLAY_TTL_MS)
        .ok_or_else(|| anyhow!("replay restart expiration overflow"))?;
    let mut seen = HashSet::with_capacity(entries.len());
    let mut rearmed = Vec::with_capacity(entries.len());
    for entry in entries {
        validate_idempotency_key(&entry.key)?;
        if !seen.insert(entry.key.clone()) {
            bail!("mutation replay state contains duplicate idempotency keys");
        }
        rearmed.push(ReplayEntry {
            key: entry.key,
            expires_at_unix_ms: rearmed_expiry,
        });
    }

    let keys = rearmed.iter().map(|entry| entry.key.clone()).collect();
    return Ok(ReplayState {
        order: VecDeque::from(rearmed),
        keys,
        last_observed_unix_ms: now_unix_ms,
    });
}

fn replay_state_from_legacy_keys(keys: Vec<String>, now_unix_ms: u64) -> Result<ReplayState> {
    let expires_at_unix_ms = now_unix_ms
        .checked_add(IDEMPOTENCY_REPLAY_TTL_MS)
        .ok_or_else(|| anyhow!("legacy replay migration expiration overflow"))?;
    let entries = keys
        .into_iter()
        .map(|key| ReplayEntry {
            key,
            expires_at_unix_ms,
        })
        .collect();
    return replay_state_from_entries(entries, now_unix_ms);
}

fn load_replay_state(path: &Path) -> Result<ReplayState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ReplayState::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot inspect mutation replay file {}", path.display())
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("mutation replay path must be a regular non-symlink file");
    }
    if metadata.len() == 0 || metadata.len() > MAX_REPLAY_FILE_BYTES {
        bail!("mutation replay file size is invalid");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("mutation replay file must not be accessible by group/other users");
        }
    }

    let bytes = fs::read(path)
        .with_context(|| format!("cannot read mutation replay file {}", path.display()))?;
    let document: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse mutation replay file {}", path.display()))?;
    let version = document
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("mutation replay file is missing a numeric version"))?;
    let now_unix_ms = unix_time_ms()?;

    return match version {
        value if value == u64::from(LEGACY_REPLAY_FILE_VERSION) => {
            let persisted: LegacyReplayFile = serde_json::from_value(document.clone())?;
            if persisted.version != LEGACY_REPLAY_FILE_VERSION {
                bail!("legacy replay version changed during parsing");
            }
            replay_state_from_legacy_keys(persisted.keys, now_unix_ms)
        }
        value if value == u64::from(REPLAY_FILE_VERSION) => {
            let persisted: ReplayFile = serde_json::from_value(document)?;
            if persisted.version != REPLAY_FILE_VERSION {
                bail!("replay version changed during parsing");
            }
            replay_state_from_entries(persisted.entries, now_unix_ms)
        }
        other => Err(anyhow!(
            "unsupported mutation replay version {other}; expected {LEGACY_REPLAY_FILE_VERSION} or {REPLAY_FILE_VERSION}"
        )),
    };
}

fn save_replay_state(path: &Path, state: &ReplayState) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("mutation replay path must be a regular non-symlink file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    bail!("mutation replay file must not be accessible by group/other users");
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot inspect mutation replay file {}", path.display())
            });
        }
    }

    let persisted = ReplayFile {
        version: REPLAY_FILE_VERSION,
        entries: state.order.iter().cloned().collect(),
    };
    let bytes = serde_json::to_vec(&persisted)?;
    if bytes.len() as u64 > MAX_REPLAY_FILE_BYTES {
        bail!("mutation replay file would exceed the configured size limit");
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("mutation replay path has no parent"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create replay directory {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create replay temp file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(&bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "cannot atomically persist mutation replay file {}",
                path.display()
            )
        })?;
    return Ok(());
}

fn is_definitive_quota_rejection(status: StatusCode, headers: &HeaderMap) -> bool {
    let decision = headers
        .get("x-ores-rate-limit-decision")
        .and_then(|value| value.to_str().ok());
    let policy = headers
        .get("x-ores-rate-limit-policy")
        .and_then(|value| value.to_str().ok());
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok());

    let matching_status = match status {
        StatusCode::TOO_MANY_REQUESTS => decision == Some("denied"),
        StatusCode::SERVICE_UNAVAILABLE => decision == Some("degraded-denied"),
        _ => false,
    };
    return matching_status
        && policy.is_some_and(|value| !value.is_empty())
        && retry_after.is_some_and(|value| !value.is_empty());
}
async fn proxy_ingress_response(
    mut upstream: reqwest::Response,
    max_bytes: usize,
) -> Result<Response> {
    if upstream
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("local ingress response exceeds {max_bytes} bytes");
    }

    let status = upstream.status();
    validate_ingress_content_encoding(upstream.headers())?;
    let headers = admitted_ingress_response_headers(upstream.headers())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            bail!("local ingress response exceeds {max_bytes} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }

    return Ok(build_ingress_response(status, headers, bytes));
}

fn validate_ingress_content_encoding(headers: &HeaderMap) -> Result<()> {
    let Some(value) = headers.get(axum::http::header::CONTENT_ENCODING) else {
        return Ok(());
    };
    let encoding = value
        .to_str()
        .context("local ingress returned a non-text content-encoding")?
        .trim();
    if encoding.eq_ignore_ascii_case("identity") {
        return Ok(());
    }
    bail!("local ingress response content-encoding is unsupported");
}

fn admitted_ingress_response_headers(upstream: &HeaderMap) -> Result<HeaderMap> {
    let mut admitted = HeaderMap::with_capacity(INGRESS_RESPONSE_HEADER_ALLOWLIST.len());
    let mut admitted_values = 0usize;
    let mut admitted_bytes = 0usize;
    for name in INGRESS_RESPONSE_HEADER_ALLOWLIST {
        let values = upstream.get_all(name);
        let mut values = values.iter();
        let Some(first) = values.next() else {
            continue;
        };
        admit_ingress_response_header_value(
            name,
            first,
            &mut admitted_values,
            &mut admitted_bytes,
        )?;
        admitted.insert(name, first.clone());
        for value in values {
            if !is_joinable_ingress_response_header(name) {
                bail!("local ingress returned duplicate {name} response headers");
            }
            admit_ingress_response_header_value(
                name,
                value,
                &mut admitted_values,
                &mut admitted_bytes,
            )?;
            admitted.append(name, value.clone());
        }
    }
    return Ok(admitted);
}

fn admit_ingress_response_header_value(
    name: &str,
    value: &axum::http::HeaderValue,
    admitted_values: &mut usize,
    admitted_bytes: &mut usize,
) -> Result<()> {
    if value.as_bytes().len() > MAX_INGRESS_RESPONSE_HEADER_VALUE_BYTES {
        bail!("local ingress response header {name} exceeds the per-value limit");
    }
    *admitted_values = admitted_values
        .checked_add(1)
        .ok_or_else(|| anyhow!("local ingress response header count overflow"))?;
    if *admitted_values > MAX_INGRESS_RESPONSE_HEADER_VALUES {
        bail!("local ingress response exceeds the admitted header value count");
    }
    *admitted_bytes = admitted_bytes
        .checked_add(name.len())
        .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
        .ok_or_else(|| anyhow!("local ingress response header byte count overflow"))?;
    if *admitted_bytes > MAX_INGRESS_RESPONSE_HEADER_BYTES {
        bail!("local ingress response exceeds the admitted header byte limit");
    }
    return Ok(());
}

fn is_joinable_ingress_response_header(name: &str) -> bool {
    return matches!(name, "cache-control" | "ratelimit" | "ratelimit-policy");
}

fn build_ingress_response(status: StatusCode, headers: HeaderMap, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    return response;
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    return difference == 0;
}

fn keep_awake_command() -> Result<(String, Vec<String>)> {
    #[cfg(target_os = "macos")]
    {
        return Ok(("caffeinate".to_owned(), vec!["-dimsu".to_owned()]));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok((
            "systemd-inhibit".to_owned(),
            vec![
                "--what=sleep".to_owned(),
                "--why=Scintilla local runtime".to_owned(),
                "sleep".to_owned(),
                "infinity".to_owned(),
            ],
        ));
    }
    #[cfg(target_os = "windows")]
    {
        return Err(anyhow!(
            "keep-awake helper is not implemented on Windows yet"
        ));
    }
    #[allow(unreachable_code)]
    return Err(anyhow!("keep-awake helper is unsupported on this OS"));
}

fn bad_request(error: impl std::fmt::Display) -> (StatusCode, String) {
    return (StatusCode::BAD_REQUEST, error.to_string());
}

fn conflict_or_internal(error: anyhow::Error) -> (StatusCode, String) {
    let message = error.to_string();
    if message.contains("already exists") || message.contains("process limit") {
        return (StatusCode::CONFLICT, message);
    }
    return internal_error(message);
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
}

fn bad_gateway(error: impl std::fmt::Display) -> (StatusCode, String) {
    return (StatusCode::BAD_GATEWAY, error.to_string());
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_process_start_gate_is_fail_closed() {
        assert!(require_client_process_start(false).is_err());
        assert!(require_client_process_start(true).is_ok());
    }

    #[test]
    fn rejects_non_loopback_bind() {
        assert!(parse_loopback_addr("0.0.0.0:8765").is_err());
        assert!(parse_loopback_addr("127.0.0.1:8765").is_ok());
    }

    #[test]
    fn ingress_origin_requires_literal_loopback_port_and_root_path() {
        assert!(require_loopback_url("http://127.0.0.1:8091").is_ok());
        assert!(require_loopback_url("http://[::1]:8091/").is_ok());
        assert!(require_loopback_url("http://127.0.0.1").is_err());
        assert!(require_loopback_url("http://localhost:8091").is_err());
        assert!(require_loopback_url("http://127.0.0.1:8091/prefix").is_err());
        assert!(require_loopback_url("http://127.0.0.1:8091@evil.example").is_err());
    }

    #[test]
    fn bearer_comparison_is_length_and_content_sensitive() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"short", b"shorter"));
    }

    #[test]
    fn rejects_unbounded_process_vectors() {
        let spec = ProcessSpec {
            name: "worker".to_owned(),
            command: "worker".to_owned(),
            args: vec!["x".to_owned(); MAX_PROCESS_ARGS + 1],
            env: BTreeMap::new(),
        };
        assert!(validate_process_spec(&spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn token_reader_rejects_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!("scintilla-token-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create token test dir");
        let target = root.join("target");
        fs::write(&target, "abcdefghijklmnopqrstuvwxyz0123456789\n").expect("write target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod target");
        let link = root.join("token");
        symlink(&target, &link).expect("create token symlink");

        assert!(read_token_file(&link).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn token_creation_is_create_new_and_round_trips() {
        let root = std::env::temp_dir().join(format!("scintilla-token-create-{}", Uuid::new_v4()));
        let path = root.join("token");
        let created = load_or_create_token(&path).expect("create token");
        let loaded = load_or_create_token(&path).expect("load token");

        assert_eq!(created, loaded);
        assert!(created.len() >= 32);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn idempotency_keys_are_bounded_ascii_tokens() {
        assert!(validate_idempotency_key("run-1234").is_ok());
        assert!(validate_idempotency_key("run:tenant/1234").is_ok());
        assert!(validate_idempotency_key("short").is_err());
        assert!(validate_idempotency_key("contains space").is_err());
        assert!(validate_idempotency_key(&"x".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1)).is_err());
    }

    #[test]
    fn ingress_proxy_preserves_quota_metadata_and_strips_sensitive_headers() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            "content-type",
            "application/problem+json".parse().expect("content type"),
        );
        upstream.insert("retry-after", "7".parse().expect("retry after"));
        upstream.insert(
            "ratelimit-policy",
            "\"tenant-minute\";q=60;w=60"
                .parse()
                .expect("rate limit policy"),
        );
        upstream.insert(
            "ratelimit",
            "\"tenant-minute\";r=0;t=7".parse().expect("rate limit"),
        );
        upstream.insert("x-request-id", "req-1234".parse().expect("request id"));
        upstream.insert("set-cookie", "secret=1".parse().expect("cookie"));
        upstream.insert(
            "authorization",
            "Bearer secret".parse().expect("authorization"),
        );
        upstream.insert("connection", "close".parse().expect("connection"));

        let admitted =
            admitted_ingress_response_headers(&upstream).expect("admit safe response headers");
        let response = build_ingress_response(
            StatusCode::TOO_MANY_REQUESTS,
            admitted,
            br#"{"code":"rate_limited"}"#.to_vec(),
        );

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("7")
        );
        assert!(response.headers().contains_key("ratelimit-policy"));
        assert!(response.headers().contains_key("ratelimit"));
        assert!(response.headers().contains_key("x-request-id"));
        assert!(!response.headers().contains_key("set-cookie"));
        assert!(!response.headers().contains_key("authorization"));
        assert!(!response.headers().contains_key("connection"));
    }

    #[test]
    fn only_canonical_quota_429_is_retry_safe() {
        let mut canonical = HeaderMap::new();
        canonical.insert(
            "x-ores-rate-limit-decision",
            "denied".parse().expect("decision"),
        );
        canonical.insert(
            "x-ores-rate-limit-policy",
            "tenant-minute".parse().expect("policy"),
        );
        canonical.insert("retry-after", "2".parse().expect("retry after"));

        assert!(is_definitive_quota_rejection(
            StatusCode::TOO_MANY_REQUESTS,
            &canonical,
        ));

        let application_429 = HeaderMap::new();
        assert!(!is_definitive_quota_rejection(
            StatusCode::TOO_MANY_REQUESTS,
            &application_429,
        ));
        assert!(!is_definitive_quota_rejection(
            StatusCode::SERVICE_UNAVAILABLE,
            &canonical,
        ));
    }
    #[test]
    fn ingress_proxy_preserves_repeated_structured_list_metadata() {
        let mut upstream = HeaderMap::new();
        upstream.append(
            "ratelimit-policy",
            "\"minute\";q=60;w=60".parse().expect("minute policy"),
        );
        upstream.append(
            "ratelimit-policy",
            "\"hour\";q=1000;w=3600".parse().expect("hour policy"),
        );

        let admitted =
            admitted_ingress_response_headers(&upstream).expect("admit list-valued metadata");
        assert_eq!(admitted.get_all("ratelimit-policy").iter().count(), 2);
    }

    #[tokio::test]
    async fn definitive_quota_rejection_releases_idempotency_reservation() {
        let root = tempfile::tempdir().expect("replay tempdir");
        let replay_path = root.path().join("mutation-replay.json");
        let state = AppState {
            token: Arc::from("abcdefghijklmnopqrstuvwxyz0123456789"),
            replay: Arc::new(Mutex::new(ReplayState::default())),
            replay_path: Arc::new(replay_path.clone()),
            processes: Arc::new(Mutex::new(HashMap::new())),
            client: reqwest::Client::new(),
            ingress_url: Arc::from("http://127.0.0.1:8091"),
            ingress_token: None,
            cloudflared_command: Arc::from("cloudflared"),
            cloudflared_args: Arc::new(Vec::new()),
            update_command: None,
            update_args: Arc::new(Vec::new()),
            allow_arbitrary_client_commands: false,
            started_at: Instant::now(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_HEADER,
            "quota-retry-0001".parse().expect("idempotency header"),
        );

        reserve_mutation(&headers, &state)
            .await
            .expect("reserve mutation");
        assert!(state.replay.lock().await.keys.contains("quota-retry-0001"));

        release_mutation(&headers, &state)
            .await
            .expect("release rejected mutation");
        assert!(!state.replay.lock().await.keys.contains("quota-retry-0001"));

        reserve_mutation(&headers, &state)
            .await
            .expect("retry may reserve the same key");
    }
    #[test]
    fn quota_rejection_classifier_requires_matching_ores_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ores-rate-limit-policy",
            "ip-default".parse().expect("policy header"),
        );
        headers.insert("retry-after", "1".parse().expect("retry header"));

        headers.insert(
            "x-ores-rate-limit-decision",
            "denied".parse().expect("decision header"),
        );
        assert!(is_definitive_quota_rejection(
            StatusCode::TOO_MANY_REQUESTS,
            &headers
        ));
        assert!(!is_definitive_quota_rejection(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers
        ));

        headers.insert(
            "x-ores-rate-limit-decision",
            "degraded-denied".parse().expect("decision header"),
        );
        assert!(is_definitive_quota_rejection(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers
        ));
        assert!(!is_definitive_quota_rejection(
            StatusCode::INTERNAL_SERVER_ERROR,
            &headers
        ));
    }

    #[test]
    fn ingress_proxy_rejects_encoded_response_bodies() {
        let mut headers = HeaderMap::new();
        assert!(validate_ingress_content_encoding(&headers).is_ok());

        headers.insert(
            axum::http::header::CONTENT_ENCODING,
            "identity".parse().expect("identity encoding"),
        );
        assert!(validate_ingress_content_encoding(&headers).is_ok());

        headers.insert(
            axum::http::header::CONTENT_ENCODING,
            "gzip".parse().expect("gzip encoding"),
        );
        assert!(validate_ingress_content_encoding(&headers).is_err());
    }

    #[test]
    fn ingress_proxy_bounds_admitted_response_headers() {
        let mut upstream = HeaderMap::new();
        let oversized = vec![b'a'; MAX_INGRESS_RESPONSE_HEADER_VALUE_BYTES + 1];
        upstream.insert(
            "x-request-id",
            axum::http::HeaderValue::from_bytes(&oversized).expect("oversized valid header value"),
        );
        assert!(admitted_ingress_response_headers(&upstream).is_err());

        let mut upstream = HeaderMap::new();
        for index in 0..=MAX_INGRESS_RESPONSE_HEADER_VALUES {
            upstream.append(
                "ratelimit-policy",
                format!("policy-{index}").parse().expect("policy value"),
            );
        }
        assert!(admitted_ingress_response_headers(&upstream).is_err());
    }
    #[test]
    fn ingress_proxy_rejects_duplicate_singleton_metadata() {
        let mut upstream = HeaderMap::new();
        upstream.append("retry-after", "2".parse().expect("retry after"));
        upstream.append("retry-after", "3".parse().expect("retry after"));

        assert!(admitted_ingress_response_headers(&upstream).is_err());
    }

    #[test]
    fn replay_state_refuses_live_eviction_and_reclaims_expired_keys() {
        let mut state = ReplayState::default();
        let now = 1_000_000;
        for index in 0..MAX_RECENT_IDEMPOTENCY_KEYS {
            state
                .reserve(format!("mutation-{index:08}"), now)
                .expect("reserve unique key");
        }
        assert_eq!(state.order.len(), MAX_RECENT_IDEMPOTENCY_KEYS);
        assert!(state.keys.contains("mutation-00000000"));

        assert!(state.reserve("mutation-overflow".to_owned(), now).is_err());
        assert!(state.keys.contains("mutation-00000000"));

        let after_expiry = now + IDEMPOTENCY_REPLAY_TTL_MS + 1;
        state
            .reserve("mutation-after-expiry".to_owned(), after_expiry)
            .expect("expired reservations are reclaimed");
        assert_eq!(state.order.len(), 1);
        assert!(!state.keys.contains("mutation-00000000"));
        assert!(state.keys.contains("mutation-after-expiry"));
    }

    #[test]
    fn replay_state_persists_across_restart_and_rejects_corruption() {
        let root = tempfile::tempdir().expect("replay tempdir");
        let path = root.path().join("mutation-replay.json");
        let mut state = ReplayState::default();
        let now = unix_time_ms().expect("current unix time");
        state
            .reserve("restart-proof-0001".to_owned(), now)
            .expect("reserve replay key");
        save_replay_state(&path, &state).expect("save replay state");

        let loaded = load_replay_state(&path).expect("load replay state");
        assert!(loaded.keys.contains("restart-proof-0001"));

        fs::write(&path, b"{not-json").expect("write corrupt replay state");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .expect("restore private permissions");
        }
        assert!(load_replay_state(&path).is_err());
    }

    #[test]
    fn persisted_replay_keys_are_rearmed_after_restart_even_if_disk_expiry_is_old() {
        let now = unix_time_ms().expect("current unix time");
        let state = replay_state_from_entries(
            vec![ReplayEntry {
                key: "restart-rearm-0001".to_owned(),
                expires_at_unix_ms: 1,
            }],
            now,
        )
        .expect("rearm persisted replay");

        let entry = state.order.front().expect("rearmed entry");
        assert_eq!(entry.key, "restart-rearm-0001");
        assert_eq!(entry.expires_at_unix_ms, now + IDEMPOTENCY_REPLAY_TTL_MS);
    }

    #[test]
    fn replay_state_migrates_v1_without_resetting_live_keys() {
        let root = tempfile::tempdir().expect("replay tempdir");
        let path = root.path().join("mutation-replay.json");
        let legacy = LegacyReplayFile {
            version: LEGACY_REPLAY_FILE_VERSION,
            keys: vec!["legacy-proof-0001".to_owned()],
        };
        fs::write(
            &path,
            serde_json::to_vec(&legacy).expect("encode legacy replay"),
        )
        .expect("write legacy replay");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .expect("chmod legacy replay");
        }

        let migrated = load_replay_state(&path).expect("migrate legacy replay");
        assert!(migrated.keys.contains("legacy-proof-0001"));
        save_replay_state(&path, &migrated).expect("persist v2 replay");

        let document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read v2 replay")).expect("parse v2");
        assert_eq!(
            document.get("version").and_then(Value::as_u64),
            Some(u64::from(REPLAY_FILE_VERSION))
        );
        assert_eq!(
            document
                .get("entries")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn replay_state_rejects_symlink_and_broad_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().expect("replay tempdir");
        let target = root.path().join("real-replay.json");
        let link = root.path().join("mutation-replay.json");
        fs::write(
            &target,
            serde_json::to_vec(&ReplayFile {
                version: REPLAY_FILE_VERSION,
                entries: vec![ReplayEntry {
                    key: "restart-proof-0002".to_owned(),
                    expires_at_unix_ms: unix_time_ms()
                        .expect("current unix time")
                        .saturating_add(IDEMPOTENCY_REPLAY_TTL_MS),
                }],
            })
            .expect("encode replay"),
        )
        .expect("write replay");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod replay");
        symlink(&target, &link).expect("symlink replay");
        assert!(load_replay_state(&link).is_err());

        fs::remove_file(&link).expect("remove replay link");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
            .expect("broaden replay permissions");
        assert!(load_replay_state(&target).is_err());
    }
}
