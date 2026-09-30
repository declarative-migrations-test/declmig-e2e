mod agent;
mod auth;
mod protocol;
mod remote;
mod worker;

use std::{net::SocketAddr, str::FromStr, time::Duration};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderName, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::time::timeout;
use tower_http::{
    request_id::{MakeRequestUuid, SetRequestIdLayer},
    trace::TraceLayer,
};
use uuid::Uuid;

use crate::{
    agent::AgentRegistry,
    protocol::{
        BrowserEngine, CreateRunRequest, ExecutionMode, ExecutionTarget, PlacementPreference,
        RunSnapshot, RunStatus, WorkerCommand, WorkerEvent, WorkerLanguage,
    },
    worker::{RunControl, RunRegistry},
};

const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_PROMPT_CHARS: usize = 50_000;
const MAX_TASK_ID_CHARS: usize = 128;
const MAX_DRIVER_METHOD_CHARS: usize = 32;
const MAX_DRIVER_PATH_CHARS: usize = 4_096;
const MAX_RUN_SECONDS: u64 = 7_200;
const WORKER_TOKEN_HEADER: &str = "x-ores-tkda-worker-token";

fn resolve_local_execution_policy(
    _address: SocketAddr,
    execution_role: Option<&str>,
    configured: Option<&str>,
) -> Result<bool, String> {
    let configured = match configured {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => {
            return Err(
                "TKDA_ALLOW_LOCAL_EXECUTION must be exactly 'true' or 'false' when set"
                    .to_string(),
            );
        }
    };

    // Explicit deployment policy is authoritative, including on desktop.
    if let Some(value) = configured {
        return Ok(value);
    }
    if execution_role == Some("desktop") {
        return Ok(true);
    }

    // Network bind scope is not execution authority. A hosted service may bind
    // loopback behind a proxy/sidecar and must not gain host-process spawning
    // simply because its listener is local.
    return Ok(false);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tkda_main_server=info,tower_http=info".into()),
        )
        .init();

    let bind = std::env::var("TKDA_BIND").unwrap_or_else(|_| "127.0.0.1:8088".to_string());
    let address = SocketAddr::from_str(&bind).expect("TKDA_BIND must be a socket address");
    let execution_role = std::env::var("TKDA_EXECUTION_ROLE").ok();
    let allow_local_execution_raw = std::env::var("TKDA_ALLOW_LOCAL_EXECUTION").ok();
    let allow_local_execution = resolve_local_execution_policy(
        address,
        execution_role.as_deref(),
        allow_local_execution_raw.as_deref(),
    )
    .expect("invalid TKDA_ALLOW_LOCAL_EXECUTION configuration");
    let desktop_role = execution_role.as_deref() == Some("desktop");
    let api_auth = auth::ApiAuth::from_env(address).expect("invalid run-control API auth configuration");

    let agents = AgentRegistry::new();
    let registry = RunRegistry::new(agents, allow_local_execution, desktop_role);
    let run_control = Router::new()
        .route("/v1/runs", post(create_run))
        .route("/v1/runs/{run_id}", get(get_run))
        .route("/v1/runs/{run_id}/cancel", post(cancel_run))
        .route("/v1/runs/{run_id}/driver", post(driver_command))
        .route("/v1/runs/{run_id}/driver/wait", post(driver_command_wait))
        .route("/v1/runs/{run_id}/ws", get(run_websocket))
        .route_layer(middleware::from_fn_with_state(
            api_auth,
            auth::enforce_api_auth,
        ));
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/v1/agents/connect", get(agent_connect))
        .route("/v1/runs/{run_id}/events", post(worker_event))
        .merge(run_control)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::new(
            HeaderName::from_static("x-request-id"),
            MakeRequestUuid,
        ))
        .with_state(registry);

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("failed to bind Takoda main server");

    tracing::info!(%address, allow_local_execution, desktop_role, "Takoda main supervisor listening");
    axum::serve(listener, app)
        .await
        .expect("Takoda main supervisor failed");
}

async fn healthz() -> impl IntoResponse {
    return Json(json!({"ok": true, "service": "tkda-main-server"}));
}

async fn agent_connect(
    websocket: WebSocketUpgrade,
    State(registry): State<RunRegistry>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let supplied = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| {
            return value.len() >= 32
                && value.len() <= 16_384
                && !value.chars().any(char::is_whitespace);
        })
        .ok_or_else(|| ApiError::unauthorized("valid device-agent bearer token is required"))?
        .to_owned();

    let agents = registry.agents();
    return Ok(websocket
        .max_message_size(MAX_REQUEST_BYTES)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            agents.websocket(socket, supplied).await;
        }));
}

fn valid_source_revision(value: &str) -> bool {
    return matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
}

fn valid_identifier(value: &str, max_chars: usize) -> bool {
    return !value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'));
}


async fn create_run(
    State(registry): State<RunRegistry>,
    Json(request): Json<CreateRunRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_create_run_request(&request)?;

    let control = registry
        .create(request)
        .await
        .map_err(ApiError::forbidden)?;
    let snapshot = control.snapshot.read().await.clone();
    return Ok((StatusCode::ACCEPTED, Json(snapshot)));
}

fn validate_create_run_request(request: &CreateRunRequest) -> Result<(), ApiError> {
    if !valid_identifier(&request.task_id, MAX_TASK_ID_CHARS) {
        return Err(ApiError::bad_request(
            "task_id must be a 1..=128 ASCII identifier",
        ));
    }
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    if request.prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(ApiError::bad_request("prompt exceeds 50000 characters"));
    }
    if !(20 * 60..=MAX_RUN_SECONDS).contains(&request.timeout_secs) {
        return Err(ApiError::bad_request("timeout_secs must be in 1200..=7200"));
    }
    if request
        .source_revision
        .as_deref()
        .is_some_and(|source_revision| !valid_source_revision(source_revision))
    {
        return Err(ApiError::bad_request(
            "source_revision must be a canonical lowercase 40- or 64-hex Git object ID",
        ));
    }
    if request.max_retries > 10 {
        return Err(ApiError::bad_request("max_retries must be <= 10"));
    }
    if request.ai.max_planning_steps > 64 || request.ai.max_replans > 8 {
        return Err(ApiError::bad_request(
            "AI planning bounds exceed server limits",
        ));
    }
    if request
        .preferred_agent_id
        .as_deref()
        .is_some_and(|value| !valid_identifier(value, 256))
    {
        return Err(ApiError::bad_request(
            "preferred_agent_id must be a 1..=256 ASCII identifier",
        ));
    }

    validate_runtime_placement(request)?;
    return Ok(());
}

fn validate_runtime_placement(request: &CreateRunRequest) -> Result<(), ApiError> {
    let is_mobile_runtime = matches!(request.language, WorkerLanguage::Javascript)
        || matches!(request.browser_engine, BrowserEngine::Webview);

    match request.placement_preference {
        PlacementPreference::Mobile => {
            if !matches!(request.execution_target, ExecutionTarget::Local) {
                return Err(ApiError::bad_request(
                    "placement_preference=mobile requires execution_target=local",
                ));
            }
            if !matches!(request.language, WorkerLanguage::Javascript)
                || !matches!(request.browser_engine, BrowserEngine::Webview)
            {
                return Err(ApiError::bad_request(
                    "mobile placement currently requires language=javascript and browser_engine=webview",
                ));
            }
            if !matches!(request.execution_mode, ExecutionMode::Headed) {
                return Err(ApiError::bad_request(
                    "mobile WebView execution requires execution_mode=headed",
                ));
            }
        }
        PlacementPreference::Desktop => {
            if !matches!(request.execution_target, ExecutionTarget::Local) {
                return Err(ApiError::bad_request(
                    "placement_preference=desktop requires execution_target=local",
                ));
            }
            if is_mobile_runtime {
                return Err(ApiError::bad_request(
                    "javascript/webview is reserved for placement_preference=mobile",
                ));
            }
        }
        PlacementPreference::Cloud => {
            if request.preferred_agent_id.is_some() {
                return Err(ApiError::bad_request(
                    "preferred_agent_id is incompatible with placement_preference=cloud",
                ));
            }
            if is_mobile_runtime {
                return Err(ApiError::bad_request(
                    "javascript/webview is not a cloud OS-worker runtime",
                ));
            }
        }
        PlacementPreference::Auto => {
            if request.preferred_agent_id.is_some() {
                return Err(ApiError::bad_request(
                    "preferred_agent_id requires explicit placement_preference=desktop or mobile so the request cannot fall back to cloud execution",
                ));
            }
            if is_mobile_runtime {
                return Err(ApiError::bad_request(
                    "javascript/webview requires explicit placement_preference=mobile",
                ));
            }
        }
    }

    return Ok(());
}
async fn get_run(
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RunSnapshot>, ApiError> {
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let snapshot = control.snapshot.read().await.clone();
    return Ok(Json(snapshot));
}

async fn cancel_run(
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RunSnapshot>, ApiError> {
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;

    {
        let mut snapshot = control.snapshot.write().await;
        if !snapshot.status.is_terminal() {
            snapshot.status = RunStatus::Cancelling;
            snapshot.last_message = Some("cancellation requested".to_string());
        }
    }
    control.cancel.cancel();
    let snapshot = control.snapshot.read().await.clone();
    return Ok(Json(snapshot));
}

async fn worker_event(
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    Json(event): Json<WorkerEvent>,
) -> Result<StatusCode, ApiError> {
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let token = headers
        .get(WORKER_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized("missing remote worker event token"))?;

    control
        .accept_remote_event(token, event)
        .await
        .map_err(ApiError::unauthorized)?;
    return Ok(StatusCode::NO_CONTENT);
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverRequest {
    method: String,
    path: String,
    #[serde(default)]
    body: serde_json::Value,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn validate_driver_parts(method: &str, path: &str) -> Result<(), ApiError> {
    if method.len() > MAX_DRIVER_METHOD_CHARS || method != "POST" {
        return Err(ApiError::bad_request("driver method must be POST"));
    }
    if path.len() > MAX_DRIVER_PATH_CHARS || path != "/webdriver" {
        return Err(ApiError::bad_request(
            "driver path must be exactly /webdriver",
        ));
    }
    Ok(())
}

fn validate_driver_request(request: &DriverRequest) -> Result<(), ApiError> {
    validate_driver_parts(&request.method, &request.path)?;
    if request
        .timeout_ms
        .is_some_and(|value| !(100..=120_000).contains(&value))
    {
        return Err(ApiError::bad_request(
            "driver timeout_ms must be in 100..=120000 when supplied",
        ));
    }
    Ok(())
}

async fn driver_command(
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
    Json(request): Json<DriverRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_driver_request(&request)?;
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;

    let lease_epoch = running_lease_epoch(&control).await?;
    let request_id = Uuid::new_v4();
    control
        .command_tx
        .send(WorkerCommand::Driver {
            request_id,
            method: request.method,
            path: request.path,
            body: request.body,
        })
        .map_err(|_| ApiError::unavailable("worker command channel is unavailable"))?;

    return Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "request_id": request_id,
            "lease_epoch": lease_epoch
        })),
    ));
}

async fn driver_command_wait(
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
    Json(request): Json<DriverRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_driver_request(&request)?;
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let lease_epoch = running_lease_epoch(&control).await?;

    let request_id = Uuid::new_v4();
    let mut events = control.event_tx.subscribe();
    control
        .command_tx
        .send(WorkerCommand::Driver {
            request_id,
            method: request.method,
            path: request.path,
            body: request.body,
        })
        .map_err(|_| ApiError::unavailable("worker command channel is unavailable"))?;

    let wait_ms = request.timeout_ms.unwrap_or(30_000);
    let response = timeout(Duration::from_millis(wait_ms), async {
        loop {
            match events.recv().await {
                Ok(WorkerEvent::DriverResponse {
                    lease_epoch: response_epoch,
                    request_id: response_id,
                    status,
                    body,
                }) if response_epoch == lease_epoch && response_id == request_id => {
                    return Ok(json!({
                        "run_id": run_id,
                        "request_id": request_id,
                        "lease_epoch": lease_epoch,
                        "status": status,
                        "body": body
                    }));
                }
                Ok(_) => {
                    let current_epoch = control.snapshot.read().await.lease_epoch;
                    if current_epoch != lease_epoch {
                        return Err(ApiError::conflict(
                            "worker attempt changed while waiting for driver response",
                        ));
                    }
                    continue;
                },
                Err(error) => {
                    return Err(ApiError::unavailable(format!(
                        "worker event channel failed: {error}"
                    )));
                }
            }
        }
    })
    .await
    .map_err(|_| ApiError::gateway_timeout("worker driver response timed out"))??;

    return Ok(Json(response));
}

async fn run_websocket(
    websocket: WebSocketUpgrade,
    State(registry): State<RunRegistry>,
    Path(run_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let control = registry
        .get(run_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let lease_epoch = running_lease_epoch(&control).await?;

    return Ok(websocket
        .max_message_size(MAX_REQUEST_BYTES)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| websocket_loop(socket, control, lease_epoch)));
}

async fn websocket_loop(
    socket: WebSocket,
    control: std::sync::Arc<RunControl>,
    lease_epoch: u64,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = control.event_tx.subscribe();

    loop {
        tokio::select! {
            inbound = receiver.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        if control.snapshot.read().await.lease_epoch != lease_epoch {
                            let _ = sender.send(Message::Text(
                                "{\"error\":\"worker attempt changed; reconnect required\"}".into()
                            )).await;
                            break;
                        }
                        match serde_json::from_str::<WorkerCommand>(&text) {
                            Ok(command @ WorkerCommand::Driver { .. }) => {
                                let valid = match &command {
                                    WorkerCommand::Driver { method, path, .. } => validate_driver_parts(method, path),
                                    _ => unreachable!(),
                                };
                                if let Err(error) = valid {
                                    let body = json!({"error": error.message}).to_string();
                                    let _ = sender.send(Message::Text(body.into())).await;
                                    continue;
                                }
                                if control.command_tx.send(command).is_err() {
                                    let _ = sender.send(Message::Text("{\"error\":\"worker channel unavailable\"}".into())).await;
                                    break;
                                }
                            }
                            Ok(command @ WorkerCommand::Replan { .. }) => {
                                let reason_ok = match &command {
                                    WorkerCommand::Replan { reason } => {
                                        reason.chars().count() <= 20_000 && !reason.chars().any(char::is_control)
                                    }
                                    _ => false,
                                };
                                if !reason_ok {
                                    let _ = sender.send(Message::Text("{\"error\":\"invalid replan reason\"}".into())).await;
                                    continue;
                                }
                                if control.command_tx.send(command).is_err() {
                                    let _ = sender.send(Message::Text("{\"error\":\"worker channel unavailable\"}".into())).await;
                                    break;
                                }
                            }
                            Ok(_) => {
                                let _ = sender.send(Message::Text("{\"error\":\"command type is not accepted on this websocket\"}".into())).await;
                            }
                            Err(error) => {
                                let body = json!({"error": "invalid command", "detail": error.to_string()}).to_string();
                                let _ = sender.send(Message::Text(body.into())).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if event.lease_epoch() != lease_epoch {
                            if control.snapshot.read().await.lease_epoch != lease_epoch {
                                let _ = sender.send(Message::Text(
                                    "{\"error\":\"worker attempt changed; reconnect required\"}".into()
                                )).await;
                                break;
                            }
                            continue;
                        }
                        if let Ok(body) = serde_json::to_string(&event)
                            && sender.send(Message::Text(body.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast_error) => {
                        tracing::debug!(?broadcast_error, "worker event subscriber lagged or closed");
                    }
                }
            }
        }
    }
}

async fn running_lease_epoch(control: &RunControl) -> Result<u64, ApiError> {
    let snapshot = control.snapshot.read().await;
    if matches!(snapshot.status, RunStatus::Running) && snapshot.lease_epoch > 0 {
        return Ok(snapshot.lease_epoch);
    }

    Err(ApiError::conflict(format!(
        "run is not accepting driver commands while status is {:?}",
        snapshot.status
    )))
}

async fn ensure_running(control: &RunControl) -> Result<(), ApiError> {
    running_lease_epoch(control).await.map(|_| ())
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        };
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        };
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        };
    }

    fn conflict(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        };
    }

    fn unavailable(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        };
    }

    fn gateway_timeout(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: message.into(),
        };
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        return (self.status, Json(json!({"error": self.message}))).into_response();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AiPolicy;

    fn request(
        language: WorkerLanguage,
        browser_engine: BrowserEngine,
        execution_target: ExecutionTarget,
        placement_preference: PlacementPreference,
        execution_mode: ExecutionMode,
    ) -> CreateRunRequest {
        return CreateRunRequest {
            task_id: "task".to_string(),
            prompt: "open example.com".to_string(),
            language,
            browser_engine,
            source_revision: None,
            execution_target,
            placement_preference,
            execution_mode,
            preferred_agent_id: None,
            timeout_secs: 1800,
            max_retries: 2,
            ai: AiPolicy::default(),
        };
    }

    #[test]
    fn mobile_runtime_requires_mobile_local_headed_placement() {
        let mobile = request(
            WorkerLanguage::Javascript,
            BrowserEngine::Webview,
            ExecutionTarget::Local,
            PlacementPreference::Mobile,
            ExecutionMode::Headed,
        );
        assert!(validate_runtime_placement(&mobile).is_ok());

        let mut wrong = mobile.clone();
        wrong.execution_mode = ExecutionMode::Headless;
        assert!(validate_runtime_placement(&wrong).is_err());

        wrong = mobile.clone();
        wrong.execution_target = ExecutionTarget::Scintilla;
        assert!(validate_runtime_placement(&wrong).is_err());

        wrong = mobile.clone();
        wrong.placement_preference = PlacementPreference::Cloud;
        assert!(validate_runtime_placement(&wrong).is_err());
    }

    #[test]
    fn desktop_and_cloud_reject_mobile_only_runtime() {
        let desktop = request(
            WorkerLanguage::Javascript,
            BrowserEngine::Webview,
            ExecutionTarget::Local,
            PlacementPreference::Desktop,
            ExecutionMode::Headed,
        );
        assert!(validate_runtime_placement(&desktop).is_err());

        let cloud = request(
            WorkerLanguage::Javascript,
            BrowserEngine::Webview,
            ExecutionTarget::Local,
            PlacementPreference::Cloud,
            ExecutionMode::Headed,
        );
        assert!(validate_runtime_placement(&cloud).is_err());
    }

    #[test]
    fn preferred_agent_requires_explicit_device_placement() {
        let mut auto = request(
            WorkerLanguage::Typescript,
            BrowserEngine::Playwright,
            ExecutionTarget::Local,
            PlacementPreference::Auto,
            ExecutionMode::Headless,
        );
        auto.preferred_agent_id = Some("device-1".to_string());
        assert!(validate_runtime_placement(&auto).is_err());

        auto.placement_preference = PlacementPreference::Desktop;
        assert!(validate_runtime_placement(&auto).is_ok());
    }
}

#[cfg(test)]
mod source_revision_tests {
    use super::*;

    #[test]
    fn source_revision_is_canonical_immutable_git_identity() {
        assert!(!valid_source_revision("main"));
        assert!(!valid_source_revision(&"a".repeat(39)));
        assert!(!valid_source_revision(&"a".repeat(41)));
        assert!(!valid_source_revision(&"A".repeat(40)));
        assert!(!valid_source_revision(&format!("{}g", "a".repeat(39))));
        assert!(valid_source_revision(&"a".repeat(40)));
        assert!(valid_source_revision(&"b".repeat(64)));
    }

    #[test]
    fn execution_identifiers_are_url_and_log_safe() {
        assert!(valid_identifier("task:desktop-1", 128));
        assert!(valid_identifier("agent_01.example", 256));
        assert!(!valid_identifier("", 128));
        assert!(!valid_identifier("task with spaces", 128));
        assert!(!valid_identifier("task/../../escape", 128));
        assert!(!valid_identifier("task\nlog-injection", 128));
    }
}

#[cfg(test)]
mod local_execution_policy_tests {
    use super::resolve_local_execution_policy;
    use std::net::SocketAddr;

    fn address(value: &str) -> SocketAddr {
        value.parse().expect("socket address")
    }

    #[test]
    fn explicit_false_overrides_desktop_role() {
        assert!(!resolve_local_execution_policy(
            address("0.0.0.0:8088"),
            Some("desktop"),
            Some("false"),
        )
        .expect("desktop deny policy"));
    }

    #[test]
    fn desktop_role_defaults_to_local_execution_when_unset() {
        assert!(resolve_local_execution_policy(
            address("0.0.0.0:8088"),
            Some("desktop"),
            None,
        )
        .expect("desktop default policy"));
    }

    #[test]
    fn explicit_true_allows_trusted_non_desktop_host_execution() {
        assert!(resolve_local_execution_policy(
            address("10.0.0.8:8088"),
            Some("cloud"),
            Some("true"),
        )
        .expect("explicit allow"));
    }

    #[test]
    fn unset_non_desktop_policy_fails_closed_regardless_of_bind_scope() {
        assert!(!resolve_local_execution_policy(
            address("127.0.0.1:8088"),
            None,
            None,
        )
        .expect("loopback default"));
        assert!(!resolve_local_execution_policy(
            address("0.0.0.0:8088"),
            Some("cloud"),
            None,
        )
        .expect("cloud default"));
    }

    #[test]
    fn invalid_policy_value_is_rejected() {
        assert!(resolve_local_execution_policy(
            address("127.0.0.1:8088"),
            Some("desktop"),
            Some("TRUE"),
        )
        .is_err());
    }


}
