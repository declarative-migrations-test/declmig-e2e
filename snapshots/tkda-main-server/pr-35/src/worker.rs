use std::{
    fs,
    future::pending,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use futures_util::StreamExt;
use tokio::{
    io::{AsyncRead, AsyncWriteExt},
    process::{Child, Command},
    sync::{RwLock, broadcast, oneshot},
    time::{Instant, interval, sleep, timeout},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};
use uuid::Uuid;

use crate::{
    agent::{AgentRegistry, DeviceClass, DeviceLease},
    protocol::{
        BrowserEngine, CreateRunRequest, ExecutionLocation, ExecutionMode, ExecutionTarget,
        PlacementPreference, RunSnapshot, RunStatus, WorkerCommand, WorkerEvent, WorkerLanguage,
    },
    remote::{RemoteHandle, RemoteLauncher},
};

const MIN_RUN_SECONDS: u64 = 20 * 60;
const MAX_RUN_SECONDS: u64 = 2 * 60 * 60;
const TERMINAL_RETENTION_SECONDS: u64 = 15 * 60;
const MAX_STATUS_MESSAGE_CHARS: usize = 20_000;
const MAX_WORKER_EVENT_LINE_BYTES: usize = 1024 * 1024;
const REMOTE_STARTUP_TIMEOUT_SECONDS: u64 = 60;
const REMOTE_HEARTBEAT_TIMEOUT_SECONDS: u64 = 45;

#[derive(Clone)]
pub struct RunRegistry {
    runs: Arc<DashMap<Uuid, Arc<RunControl>>>,
    agents: AgentRegistry,
    allow_local_process_execution: bool,
    desktop_role: bool,
}

#[derive(Clone, Copy)]
enum ExecutionPlan {
    Local,
    Device,
    Remote,
}

pub struct RunControl {
    pub snapshot: RwLock<RunSnapshot>,
    pub command_tx: broadcast::Sender<WorkerCommand>,
    pub event_tx: broadcast::Sender<WorkerEvent>,
    pub cancel: CancellationToken,
    remote_event_token: RwLock<Option<String>>,
    last_heartbeat: RwLock<Option<Instant>>,
}

fn constant_time_token_eq(expected: &[u8], presented: &[u8]) -> bool {
    let width = expected.len().max(presented.len());
    let mut different = expected.len() ^ presented.len();
    for index in 0..width {
        let expected_byte = expected.get(index).copied().unwrap_or_default();
        let presented_byte = presented.get(index).copied().unwrap_or_default();
        different |= usize::from(expected_byte ^ presented_byte);
    }
    different == 0
}

impl RunControl {
    pub async fn accept_remote_event(
        &self,
        supplied_token: &str,
        event: WorkerEvent,
    ) -> Result<(), String> {
        let token = self.remote_event_token.read().await;
        let valid = token.as_deref().is_some_and(|expected| {
            constant_time_token_eq(expected.as_bytes(), supplied_token.as_bytes())
        });
        if !valid {
            return Err("worker event token is invalid or expired".to_string());
        }
        drop(token);

        self.validate_event_epoch(&event).await?;
        self.record_worker_event(&event).await;
        let _ = self.event_tx.send(event);
        return Ok(());
    }

    async fn validate_event_epoch(&self, event: &WorkerEvent) -> Result<(), String> {
        let event_epoch = event.lease_epoch();
        let snapshot = self.snapshot.read().await;
        let active_epoch = snapshot.lease_epoch;
        if event_epoch == 0 || active_epoch == 0 || event_epoch != active_epoch {
            return Err(format!(
                "worker event lease_epoch {event_epoch} does not match active lease_epoch {active_epoch}"
            ));
        }
        Ok(())
    }

    async fn record_worker_event(&self, event: &WorkerEvent) {
        match event {
            WorkerEvent::Ready { .. } => {
                self.touch_heartbeat().await;
                let mut snapshot = self.snapshot.write().await;
                if matches!(snapshot.status, RunStatus::Starting | RunStatus::Retrying) {
                    snapshot.status = RunStatus::Running;
                }
                snapshot.last_message = Some("worker ready".to_string());
            }
            WorkerEvent::Heartbeat { .. } => {
                self.touch_heartbeat().await;
            }
            WorkerEvent::Log { message, .. } => {
                let mut snapshot = self.snapshot.write().await;
                snapshot.last_message = Some(truncate_message(message));
            }
            WorkerEvent::Checkpoint { name, .. } => {
                let mut snapshot = self.snapshot.write().await;
                snapshot.last_message = Some(format!("checkpoint: {}", truncate_message(name)));
            }
            WorkerEvent::NeedsReplan { reason, .. } => {
                let mut snapshot = self.snapshot.write().await;
                snapshot.last_message =
                    Some(format!("replan requested: {}", truncate_message(reason)));
            }
            WorkerEvent::Completed { .. } => {
                let mut snapshot = self.snapshot.write().await;
                snapshot.last_message = Some("worker completed".to_string());
            }
            WorkerEvent::Failed { error, .. } => {
                let mut snapshot = self.snapshot.write().await;
                snapshot.last_message = Some(truncate_message(error));
            }
            WorkerEvent::DriverResponse { .. } => {}
        }
    }

    async fn touch_heartbeat(&self) {
        *self.last_heartbeat.write().await = Some(Instant::now());
        let mut snapshot = self.snapshot.write().await;
        snapshot.last_heartbeat_unix_ms = Some(unix_time_ms());
    }

    async fn set_remote_event_token(&self, token: Option<String>) {
        *self.remote_event_token.write().await = token;
    }

    async fn heartbeat_age(&self) -> Option<Duration> {
        let heartbeat = self.last_heartbeat.read().await;
        return heartbeat.map(|instant| instant.elapsed());
    }
}

impl RunRegistry {
    pub fn new(
        agents: AgentRegistry,
        allow_local_process_execution: bool,
        desktop_role: bool,
    ) -> Self {
        Self {
            runs: Arc::new(DashMap::new()),
            agents,
            allow_local_process_execution,
            desktop_role,
        }
    }

    pub fn agents(&self) -> AgentRegistry {
        self.agents.clone()
    }

    fn execution_plan(&self, request: &CreateRunRequest) -> Result<ExecutionPlan, String> {
        if self.desktop_role {
            if request.execution_target != ExecutionTarget::Local {
                return Err(
                    "desktop execution role accepts only execution_target=local".to_string(),
                );
            }
            if matches!(
                request.placement_preference,
                PlacementPreference::Cloud | PlacementPreference::Mobile
            ) {
                return Err(
                    "desktop execution role cannot satisfy cloud or mobile placement".to_string(),
                );
            }
            return Ok(ExecutionPlan::Local);
        }

        Ok(match (&request.placement_preference, &request.execution_target) {
            (PlacementPreference::Desktop | PlacementPreference::Mobile, _) => {
                ExecutionPlan::Device
            }
            (PlacementPreference::Cloud, ExecutionTarget::Local) => ExecutionPlan::Local,
            (PlacementPreference::Cloud, ExecutionTarget::Scintilla) => ExecutionPlan::Remote,
            (PlacementPreference::Auto, _) if self.agents.has_matching_agent(request) => {
                ExecutionPlan::Device
            }
            (PlacementPreference::Auto, ExecutionTarget::Local) => ExecutionPlan::Local,
            (PlacementPreference::Auto, ExecutionTarget::Scintilla) => ExecutionPlan::Remote,
        })
    }

    pub async fn create(
        &self,
        request: CreateRunRequest,
    ) -> Result<Arc<RunControl>, String> {
        if !(MIN_RUN_SECONDS..=MAX_RUN_SECONDS).contains(&request.timeout_secs) {
            return Err(format!(
                "timeout_secs must be in {MIN_RUN_SECONDS}..={MAX_RUN_SECONDS}"
            ));
        }
        let plan = self.execution_plan(&request)?;
        if matches!(plan, ExecutionPlan::Local) && !self.allow_local_process_execution {
            return Err(
                "local host-process execution is disabled on this supervisor; use an authenticated device agent or Scintilla execution"
                    .to_string(),
            );
        }

        let run_id = Uuid::new_v4();
        let (command_tx, _) = broadcast::channel(256);
        let (event_tx, _) = broadcast::channel(512);

        let control = Arc::new(RunControl {
            snapshot: RwLock::new(RunSnapshot {
                run_id,
                task_id: request.task_id.clone(),
                language: request.language.clone(),
                browser_engine: request.browser_engine.clone(),
                execution_target: request.execution_target.clone(),
                placement_preference: request.placement_preference.clone(),
                execution_mode: request.execution_mode.clone(),
                execution_location: None,
                preferred_agent_id: request.preferred_agent_id.clone(),
                executor_id: None,
                status: RunStatus::Queued,
                attempt: 0,
                max_retries: request.max_retries,
                timeout_secs: request.timeout_secs,
                pid: None,
                execution_id: None,
                lease_epoch: 0,
                last_heartbeat_unix_ms: None,
                last_message: None,
            }),
            command_tx,
            event_tx,
            cancel: CancellationToken::new(),
            remote_event_token: RwLock::new(None),
            last_heartbeat: RwLock::new(None),
        });

        self.runs.insert(run_id, control.clone());
        let registry = self.clone();
        let supervised_control = control.clone();
        let allow_local_process_execution = self.allow_local_process_execution;
        let desktop_role = self.desktop_role;
        tokio::spawn(async move {
            match plan {
                ExecutionPlan::Local => {
                    supervise_local(
                        supervised_control,
                        request,
                        allow_local_process_execution,
                        desktop_role,
                    )
                    .await;
                }
                ExecutionPlan::Device => {
                    supervise_device(supervised_control, request, registry.agents.clone()).await;
                }
                ExecutionPlan::Remote => {
                    supervise_remote(supervised_control, request).await;
                }
            }

            sleep(Duration::from_secs(TERMINAL_RETENTION_SECONDS)).await;
            registry.runs.remove(&run_id);
        });

        Ok(control)
    }

    pub fn get(&self, run_id: Uuid) -> Option<Arc<RunControl>> {
        self.runs.get(&run_id).map(|entry| entry.value().clone())
    }
}

async fn supervise_local(
    control: Arc<RunControl>,
    request: CreateRunRequest,
    allow_local_process_execution: bool,
    desktop_role: bool,
) {
    {
        let mut snapshot = control.snapshot.write().await;
        snapshot.execution_location = Some(if desktop_role {
            ExecutionLocation::Desktop
        } else {
            ExecutionLocation::Cloud
        });
        if desktop_role {
            snapshot.executor_id = std::env::var("TKDA_AGENT_ID").ok();
        }
    }

    let attempts = request.max_retries.saturating_add(1);
    let deadline = Instant::now() + Duration::from_secs(request.timeout_secs);

    for attempt in 1..=attempts {
        if control.cancel.is_cancelled() {
            set_status(
                &control,
                RunStatus::Cancelled,
                Some("cancelled before launch"),
            )
            .await;
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            set_status(
                &control,
                RunStatus::TimedOut,
                Some("global run budget exhausted before launch"),
            )
            .await;
            return;
        }

        prepare_attempt(&control, attempt, attempts).await;
        let lease_epoch = control.snapshot.read().await.lease_epoch;

        let (child, runtime_dir) = match launch_worker(
            &request,
            control.snapshot.read().await.run_id,
            allow_local_process_execution,
            desktop_role,
        )
        .await
        {
            Ok(launched) => launched,
            Err(error) => {
                let message = format!("worker launch failed: {error}");
                if attempt < attempts {
                    set_status(&control, RunStatus::Retrying, Some(&message)).await;
                    continue;
                }

                set_status(&control, RunStatus::Failed, Some(&message)).await;
                return;
            }
        };

        let outcome = run_local_attempt(
            control.clone(),
            request.clone(),
            child,
            remaining,
            lease_epoch,
        )
        .await;
        if let Err(error) = fs::remove_dir_all(&runtime_dir) {
            tracing::warn!(
                %error,
                runtime_dir = %runtime_dir.display(),
                "failed to remove isolated worker runtime directory"
            );
        }

        match outcome {
            AttemptOutcome::Succeeded => {
                set_status(
                    &control,
                    RunStatus::Succeeded,
                    Some("worker exited successfully"),
                )
                .await;
                return;
            }
            AttemptOutcome::Cancelled => {
                set_status(&control, RunStatus::Cancelled, Some("run cancelled")).await;
                return;
            }
            AttemptOutcome::TimedOut => {
                set_status(
                    &control,
                    RunStatus::TimedOut,
                    Some("global run budget exhausted"),
                )
                .await;
                return;
            }
            AttemptOutcome::Failed { error, retryable } => {
                if retryable && attempt < attempts && Instant::now() < deadline {
                    set_status(&control, RunStatus::Retrying, Some(&error)).await;
                    continue;
                }

                set_status(&control, RunStatus::Failed, Some(&error)).await;
                return;
            }
        }
    }
}

async fn supervise_device(
    control: Arc<RunControl>,
    request: CreateRunRequest,
    agents: AgentRegistry,
) {
    let attempts = request.max_retries.saturating_add(1);
    let deadline = Instant::now() + Duration::from_secs(request.timeout_secs);

    for attempt in 1..=attempts {
        if control.cancel.is_cancelled() {
            set_status(
                &control,
                RunStatus::Cancelled,
                Some("cancelled before device lease"),
            )
            .await;
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            set_status(
                &control,
                RunStatus::TimedOut,
                Some("global run budget exhausted before device lease"),
            )
            .await;
            return;
        }

        prepare_attempt(&control, attempt, attempts).await;
        let lease_epoch = control.snapshot.read().await.lease_epoch;
        let mut device_request = request.clone();
        device_request.execution_target = ExecutionTarget::Local;
        let lease = match agents.lease(&device_request).await {
            Ok(lease) => lease,
            Err(error) => {
                if attempt < attempts {
                    set_status(&control, RunStatus::Retrying, Some(&error)).await;
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
                set_status(&control, RunStatus::Failed, Some(&error)).await;
                return;
            }
        };

        let location = match lease.device_class {
            DeviceClass::Desktop => ExecutionLocation::Desktop,
            DeviceClass::Mobile => ExecutionLocation::Mobile,
        };
        let prefix = match lease.device_class {
            DeviceClass::Desktop => "desktop",
            DeviceClass::Mobile => "mobile",
        };
        {
            let mut snapshot = control.snapshot.write().await;
            snapshot.execution_location = Some(location);
            snapshot.executor_id = Some(lease.agent_id.clone());
            snapshot.execution_id = Some(format!(
                "{prefix}:{}:{}:{}",
                lease.agent_id, lease.lease_id, lease.local_run_id
            ));
            snapshot.last_message = Some(format!(
                "{prefix} lease accepted by {}; local run {}",
                lease.agent_id, lease.local_run_id
            ));
        }

        let forwarder =
            spawn_device_command_forwarder(agents.clone(), &lease, control.clone(), lease_epoch);
        let outcome = run_device_attempt(control.clone(), &agents, &lease, remaining).await;
        forwarder.abort();
        agents.release(&lease);

        match outcome {
            AttemptOutcome::Succeeded => {
                set_status(
                    &control,
                    RunStatus::Succeeded,
                    Some("device worker completed successfully"),
                )
                .await;
                return;
            }
            AttemptOutcome::Cancelled => {
                set_status(&control, RunStatus::Cancelled, Some("run cancelled")).await;
                return;
            }
            AttemptOutcome::TimedOut => {
                set_status(
                    &control,
                    RunStatus::TimedOut,
                    Some("global run budget exhausted"),
                )
                .await;
                return;
            }
            AttemptOutcome::Failed { error, retryable } => {
                if retryable && attempt < attempts && Instant::now() < deadline {
                    set_status(&control, RunStatus::Retrying, Some(&error)).await;
                    continue;
                }
                set_status(&control, RunStatus::Failed, Some(&error)).await;
                return;
            }
        }
    }
}

fn spawn_device_command_forwarder(
    agents: AgentRegistry,
    lease: &DeviceLease,
    control: Arc<RunControl>,
    lease_epoch: u64,
) -> tokio::task::JoinHandle<()> {
    let agent_id = lease.agent_id.clone();
    let local_run_id = lease.local_run_id.clone();
    let mut commands = control.command_tx.subscribe();

    return tokio::spawn(async move {
        while let Ok(command) = commands.recv().await {
            match command {
                WorkerCommand::Driver {
                    request_id,
                    method,
                    path,
                    body,
                } => {
                    let response = agents
                        .rpc(
                            &agent_id,
                            "POST",
                            &format!("/v1/runs/{local_run_id}/driver/wait"),
                            serde_json::json!({
                                "method": method,
                                "path": path,
                                "body": body,
                                "timeout_ms": 120_000
                            }),
                        )
                        .await;

                    let event = match response {
                        Ok(response) if (200..300).contains(&response.status) => {
                            WorkerEvent::DriverResponse {
                                lease_epoch,
                                request_id,
                                status: response
                                    .body
                                    .get("status")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(500)
                                    .min(u64::from(u16::MAX))
                                    as u16,
                                body: response
                                    .body
                                    .get("body")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            }
                        }
                        Ok(response) => WorkerEvent::DriverResponse {
                                lease_epoch,
                            request_id,
                            status: response.status,
                            body: response.body,
                        },
                        Err(error) => WorkerEvent::DriverResponse {
                                lease_epoch,
                            request_id,
                            status: 503,
                            body: serde_json::json!({"error": error}),
                        },
                    };
                    let _ = control.event_tx.send(event);
                }
                WorkerCommand::Cancel { reason } => {
                    let _ = agents
                        .rpc(
                            &agent_id,
                            "POST",
                            &format!("/v1/runs/{local_run_id}/cancel"),
                            serde_json::json!({"reason": reason}),
                        )
                        .await;
                }
                WorkerCommand::Replan { reason } => {
                    let _ = control.event_tx.send(WorkerEvent::Log {
                        lease_epoch,
                        level: "warn".to_string(),
                        message: format!(
                            "device supervisor does not expose HTTP replan yet: {}",
                            truncate_message(&reason)
                        ),
                    });
                }
                WorkerCommand::Start { .. } => {}
            }
        }
    });
}

async fn run_device_attempt(
    control: Arc<RunControl>,
    agents: &AgentRegistry,
    lease: &DeviceLease,
    attempt_budget: Duration,
) -> AttemptOutcome {
    let startup_deadline = Instant::now() + Duration::from_secs(REMOTE_STARTUP_TIMEOUT_SECONDS);
    let budget = sleep(attempt_budget);
    tokio::pin!(budget);
    let mut poll = interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = control.cancel.cancelled() => {
                let _ = agents
                    .rpc(
                        &lease.agent_id,
                        "POST",
                        &format!("/v1/runs/{}/cancel", lease.local_run_id),
                        serde_json::json!({"reason": "cloud supervisor cancellation"}),
                    )
                    .await;
                return AttemptOutcome::Cancelled;
            }
            _ = &mut budget => {
                let _ = agents
                    .rpc(
                        &lease.agent_id,
                        "POST",
                        &format!("/v1/runs/{}/cancel", lease.local_run_id),
                        serde_json::json!({"reason": "cloud supervisor timeout"}),
                    )
                    .await;
                return AttemptOutcome::TimedOut;
            }
            _ = poll.tick() => {
                let response = match agents
                    .rpc(
                        &lease.agent_id,
                        "GET",
                        &format!("/v1/runs/{}", lease.local_run_id),
                        serde_json::Value::Null,
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        return AttemptOutcome::Failed {
                            retryable: false,
                            error: format!(
                                "device run status is ambiguous; refusing automatic retry because the leased run may still be active: {error}"
                            ),
                        };
                    }
                };
                if !(200..300).contains(&response.status) {
                    return AttemptOutcome::Failed {
                        retryable: false,
                        error: format!(
                            "device run status returned ambiguous HTTP {}; refusing automatic retry because the leased run may still be active: {}",
                            response.status, response.body
                        ),
                    };
                }

                let status = response
                    .body
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let message = response
                    .body
                    .get("last_message")
                    .and_then(serde_json::Value::as_str)
                    .map(truncate_message);

                match status {
                    "queued" | "starting" | "retrying" => {
                        if Instant::now() >= startup_deadline {
                            return AttemptOutcome::Failed {
                                retryable: false,
                                error: format!(
                                    "device worker readiness is ambiguous after {REMOTE_STARTUP_TIMEOUT_SECONDS}s; refusing automatic retry because the leased run may still start"
                                ),
                            };
                        }
                    }
                    "running" => {
                        control.touch_heartbeat().await;
                        let mut snapshot = control.snapshot.write().await;
                        snapshot.status = RunStatus::Running;
                        if message.is_some() {
                            snapshot.last_message = message;
                        }
                    }
                    "succeeded" => {
                        return AttemptOutcome::Succeeded;
                    }
                    "cancelled" => {
                        return AttemptOutcome::Failed {
                            retryable: false,
                            error: message.unwrap_or_else(|| "device run was cancelled".to_string()),
                        };
                    }
                    "timed_out" => {
                        return AttemptOutcome::TimedOut;
                    }
                    "failed" => {
                        return AttemptOutcome::Failed {
                            retryable: true,
                            error: message.unwrap_or_else(|| "device run failed".to_string()),
                        };
                    }
                    other => {
                        return AttemptOutcome::Failed {
                            retryable: false,
                            error: format!(
                                "device run returned ambiguous status {other:?}; refusing automatic retry"
                            ),
                        };
                    }
                }
            }
        }
    }
}

async fn supervise_remote(control: Arc<RunControl>, request: CreateRunRequest) {
    {
        let mut snapshot = control.snapshot.write().await;
        snapshot.execution_location = Some(ExecutionLocation::Cloud);
    }

    let launcher = match RemoteLauncher::from_env() {
        Ok(launcher) => launcher,
        Err(error) => {
            set_status(&control, RunStatus::Failed, Some(&error)).await;
            return;
        }
    };
    let attempts = request.max_retries.saturating_add(1);
    let deadline = Instant::now() + Duration::from_secs(request.timeout_secs);
    let run_id = control.snapshot.read().await.run_id;

    for attempt in 1..=attempts {
        if control.cancel.is_cancelled() {
            set_status(
                &control,
                RunStatus::Cancelled,
                Some("cancelled before remote launch"),
            )
            .await;
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            set_status(
                &control,
                RunStatus::TimedOut,
                Some("global run budget exhausted before remote launch"),
            )
            .await;
            return;
        }

        prepare_attempt(&control, attempt, attempts).await;
        let lease_epoch = control.snapshot.read().await.lease_epoch;
        let event_token = Uuid::new_v4().to_string();
        control
            .set_remote_event_token(Some(event_token.clone()))
            .await;
        *control.last_heartbeat.write().await = None;

        let mut events = control.event_tx.subscribe();
        let handle = match launcher
            .launch(run_id, lease_epoch, &request, &event_token)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                control.set_remote_event_token(None).await;
                if attempt < attempts {
                    set_status(&control, RunStatus::Retrying, Some(&error)).await;
                    continue;
                }

                set_status(&control, RunStatus::Failed, Some(&error)).await;
                return;
            }
        };

        {
            let mut snapshot = control.snapshot.write().await;
            snapshot.executor_id = Some(handle.execution_id.clone());
            snapshot.execution_id = Some(handle.execution_id.clone());
            snapshot.last_message = Some(format!(
                "remote worker launched; awaiting ready handshake for lease epoch {}",
                snapshot.lease_epoch
            ));
        }

        let command_forwarder = spawn_remote_command_forwarder(
            launcher.clone(),
            handle.clone(),
            control.clone(),
            lease_epoch,
        );
        let _ = control
            .command_tx
            .send(start_command(run_id, lease_epoch, &request));

        let outcome = run_remote_attempt(control.clone(), &mut events, remaining).await;
        if let Some(reason) = remote_cleanup_reason(&outcome)
            && let Err(error) = launcher
                .send_command(
                    &handle,
                    &WorkerCommand::Cancel {
                        reason: reason.to_string(),
                    },
                )
                .await
        {
            tracing::warn!(
                %error,
                execution_id = %handle.execution_id,
                lease_epoch,
                "failed to explicitly terminate remote attempt"
            );
        }
        command_forwarder.abort();
        control.set_remote_event_token(None).await;

        match outcome {
            AttemptOutcome::Succeeded => {
                set_status(
                    &control,
                    RunStatus::Succeeded,
                    Some("remote worker completed successfully"),
                )
                .await;
                return;
            }
            AttemptOutcome::Cancelled => {
                set_status(&control, RunStatus::Cancelled, Some("run cancelled")).await;
                return;
            }
            AttemptOutcome::TimedOut => {
                set_status(
                    &control,
                    RunStatus::TimedOut,
                    Some("global run budget exhausted"),
                )
                .await;
                return;
            }
            AttemptOutcome::Failed { error, retryable } => {
                if retryable && attempt < attempts && Instant::now() < deadline {
                    set_status(&control, RunStatus::Retrying, Some(&error)).await;
                    continue;
                }

                set_status(&control, RunStatus::Failed, Some(&error)).await;
                return;
            }
        }
    }
}

fn spawn_remote_command_forwarder(
    launcher: RemoteLauncher,
    handle: RemoteHandle,
    control: Arc<RunControl>,
    lease_epoch: u64,
) -> tokio::task::JoinHandle<()> {
    let mut commands = control.command_tx.subscribe();
    return tokio::spawn(async move {
        while let Ok(command) = commands.recv().await {
            if let Err(error) = launcher.send_command(&handle, &command).await {
                let _ = control.event_tx.send(WorkerEvent::Failed {
                    lease_epoch,
                    retryable: true,
                    error,
                });
                break;
            }
        }
    });
}

async fn run_remote_attempt(
    control: Arc<RunControl>,
    events: &mut broadcast::Receiver<WorkerEvent>,
    attempt_budget: Duration,
) -> AttemptOutcome {
    let startup_deadline = Instant::now() + Duration::from_secs(REMOTE_STARTUP_TIMEOUT_SECONDS);
    let budget = sleep(attempt_budget);
    tokio::pin!(budget);
    let mut watchdog = interval(Duration::from_secs(5));
    let mut ready = false;

    loop {
        tokio::select! {
            _ = control.cancel.cancelled() => {
                let _ = control.command_tx.send(WorkerCommand::Cancel {
                    reason: "supervisor cancellation".to_string(),
                });
                return AttemptOutcome::Cancelled;
            }
            _ = &mut budget => {
                let _ = control.command_tx.send(WorkerCommand::Cancel {
                    reason: "global run timeout".to_string(),
                });
                return AttemptOutcome::TimedOut;
            }
            _ = watchdog.tick() => {
                if !ready && Instant::now() >= startup_deadline {
                    return AttemptOutcome::Failed {
                        retryable: true,
                        error: format!(
                            "remote worker did not become ready within {REMOTE_STARTUP_TIMEOUT_SECONDS}s"
                        ),
                    };
                }
                if ready
                    && let Some(age) = control.heartbeat_age().await
                    && age > Duration::from_secs(REMOTE_HEARTBEAT_TIMEOUT_SECONDS)
                {
                    return AttemptOutcome::Failed {
                        retryable: true,
                        error: format!(
                            "remote worker heartbeat expired after {}s",
                            age.as_secs()
                        ),
                    };
                }
            }
            event = events.recv() => {
                match event {
                    Ok(WorkerEvent::Ready { .. }) => {
                        ready = true;
                    }
                    Ok(WorkerEvent::Heartbeat { .. }) => {}
                    Ok(WorkerEvent::Completed { .. }) => {
                        return AttemptOutcome::Succeeded;
                    }
                    Ok(WorkerEvent::Failed {
                        retryable, error, ..
                    }) => {
                        return AttemptOutcome::Failed { retryable, error };
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return AttemptOutcome::Failed {
                            retryable: true,
                            error: format!("remote event channel failed: {error}"),
                        };
                    }
                }
            }
        }
    }
}

async fn prepare_attempt(control: &RunControl, attempt: u32, attempts: u32) {
    let mut snapshot = control.snapshot.write().await;
    snapshot.attempt = attempt;
    snapshot.status = if attempt == 1 {
        RunStatus::Starting
    } else {
        RunStatus::Retrying
    };
    snapshot.pid = None;
    snapshot.execution_id = None;
    snapshot.lease_epoch = snapshot.lease_epoch.saturating_add(1);
    snapshot.last_heartbeat_unix_ms = None;
    snapshot.last_message = Some(format!("launching attempt {attempt}/{attempts}"));
}

fn copy_env_if_present(command: &mut Command, key: &str) {
    if let Ok(value) = std::env::var(key) {
        command.env(key, value);
    }
}

fn require_real_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("runtime path is not a real directory: {}", path.display()),
        ));
    }
    return Ok(());
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            require_real_directory(path)?;
        }
        Err(error) => return Err(error),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    return require_real_directory(path);
}

fn create_worker_runtime_dir(run_id: Uuid) -> std::io::Result<PathBuf> {
    let temp_root = std::env::temp_dir();
    require_real_directory(&temp_root)?;

    let product_root = temp_root.join("takoda-worker");
    create_private_directory(&product_root)?;

    let run_root = product_root.join(run_id.to_string());
    create_private_directory(&run_root)?;

    let root = run_root.join(Uuid::new_v4().to_string());
    create_private_directory(&root)?;
    create_private_directory(&root.join("config"))?;
    create_private_directory(&root.join("cache"))?;
    create_private_directory(&root.join("data"))?;
    create_private_directory(&root.join("runtime"))?;

    return Ok(root);
}

fn validate_configured_program(key: &str, value: &str) -> std::io::Result<()> {
    let path = Path::new(value);
    let contains_separator = value.contains('/') || value.contains('\\');

    if contains_separator && !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{key} paths must be absolute; otherwise use a bare executable name"),
        ));
    }
    if path.is_absolute() {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{key} must reference a regular non-symlink file"),
            ));
        }
        return Ok(());
    }
    if value.is_empty()
        || value.len() > 256
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{key} must be a bounded bare executable name or absolute regular file"),
        ));
    }
    return Ok(());
}

fn configured_program(key: &str, fallback: &str) -> std::io::Result<String> {
    match std::env::var(key) {
        Ok(value) => {
            validate_configured_program(key, &value)?;
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(fallback.to_owned()),
        Err(error) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{key} is not valid Unicode: {error}"),
        )),
    }
}

fn configured_entry(key: &str, fallback: &str) -> std::io::Result<String> {
    match std::env::var(key) {
        Ok(value) => {
            let path = Path::new(&value);
            if !path.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{key} must be an absolute file path when configured"),
                ));
            }
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{key} must reference a regular non-symlink file"),
                ));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(fallback.to_owned()),
        Err(error) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{key} is not valid Unicode: {error}"),
        )),
    }
}

fn set_worker_private_paths(command: &mut Command, runtime_dir: &Path) {
    command
        .env("HOME", runtime_dir)
        .env("USERPROFILE", runtime_dir)
        .env("TMPDIR", runtime_dir)
        .env("TMP", runtime_dir)
        .env("TEMP", runtime_dir)
        .env("XDG_CONFIG_HOME", runtime_dir.join("config"))
        .env("XDG_CACHE_HOME", runtime_dir.join("cache"))
        .env("XDG_DATA_HOME", runtime_dir.join("data"))
        .env("XDG_RUNTIME_DIR", runtime_dir.join("runtime"));
}

fn validate_selenium_upstream(value: &str) -> std::io::Result<()> {
    let authority = value.strip_prefix("http://").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TKDA_SELENIUM_UPSTREAM_URL must use literal-loopback HTTP",
        )
    })?;
    if authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
        || authority.contains('@')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TKDA_SELENIUM_UPSTREAM_URL must be a root origin without credentials, path, query, or fragment",
        ));
    }
    let address: SocketAddr = authority.parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TKDA_SELENIUM_UPSTREAM_URL must include an explicit loopback port",
        )
    })?;
    let literal_loopback = matches!(
        address.ip(),
        IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST
    ) || matches!(
        address.ip(),
        IpAddr::V6(ip) if ip == Ipv6Addr::LOCALHOST
    );
    if !literal_loopback || address.port() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TKDA_SELENIUM_UPSTREAM_URL must target 127.0.0.1 or ::1 with a nonzero port",
        ));
    }
    Ok(())
}

fn apply_worker_environment(
    command: &mut Command,
    language: &WorkerLanguage,
    runtime_dir: &Path,
    desktop_role: bool,
) -> std::io::Result<()> {
    command.env_clear();
    set_worker_private_paths(command, runtime_dir);

    for key in [
        "PATH",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
        "SYSTEMROOT",
        "WINDIR",
        "LANG",
        "LC_ALL",
    ] {
        copy_env_if_present(command, key);
    }

    for key in [
        "TKDA_ALLOW_HEADED",
        "TKDA_ALLOW_EVALUATE",
        "TKDA_ALLOW_SELENIUM_EVALUATE",
        "TKDA_SELENIUM_BROWSER",
        "TKDA_MAX_TEXT_CHARS",
        "TKDA_MAX_SCREENSHOT_BYTES",
        "TKDA_ACTION_RETRIES",
        "TKDA_BROWSER_ALLOWED_PORTS",
    ] {
        copy_env_if_present(command, key);
    }

    if let Ok(value) = std::env::var("TKDA_SELENIUM_UPSTREAM_URL") {
        validate_selenium_upstream(&value)?;
        command.env("TKDA_SELENIUM_UPSTREAM_URL", value);
    }

    if matches!(language, WorkerLanguage::Typescript) {
        for key in [
            "TKDA_AI_BASE_URL",
            "TKDA_AI_API_KEY",
            "TKDA_AI_MODEL",
            "TKDA_BROWSER_ALLOWED_DOMAINS",
        ] {
            copy_env_if_present(command, key);
        }

        if desktop_role {
            copy_env_if_present(command, "TKDA_PLAYWRIGHT_USER_DATA_DIR");
        }
    }

    Ok(())
}

async fn launch_worker(
    request: &CreateRunRequest,
    run_id: Uuid,
    allow_local_process_execution: bool,
    desktop_role: bool,
) -> std::io::Result<(Child, PathBuf)> {
    if !allow_local_process_execution {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "local host-process execution is disabled on this supervisor",
        ));
    }

    if matches!(request.language, WorkerLanguage::Javascript)
        || matches!(request.browser_engine, BrowserEngine::Webview)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "javascript + webview is a mobile device-agent runtime and cannot launch as a local OS worker",
        ));
    }

    if matches!(request.execution_mode, ExecutionMode::Headed)
        && std::env::var("TKDA_ALLOW_HEADED").as_deref() != Ok("true")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "headed execution is disabled on this supervisor",
        ));
    }

    let runtime_dir = create_worker_runtime_dir(run_id)?;

    let mut command = match request.language {
        WorkerLanguage::Typescript => {
            let executable = configured_program("TKDA_TYPESCRIPT_WORKER_CMD", "node")?;
            let entry = configured_entry(
                "TKDA_TYPESCRIPT_WORKER_ENTRY",
                "../tkda-browser-workers.ts/dist/worker.js",
            )?;
            let mut command = Command::new(executable);
            command.arg(entry);
            command
        }
        WorkerLanguage::Rust => Command::new(configured_program(
            "TKDA_RUST_WORKER_CMD",
            "workers/rust/target/release/tkda-rust-worker",
        )?),
        WorkerLanguage::Go => Command::new(configured_program(
            "TKDA_GO_WORKER_CMD",
            "workers/go/tkda-go-worker",
        )?),
        WorkerLanguage::Python => {
            let executable = configured_program("TKDA_PYTHON_WORKER_CMD", "python3")?;
            let entry = configured_entry(
                "TKDA_PYTHON_WORKER_ENTRY",
                "workers/python/tkda_worker.py",
            )?;
            let mut command = Command::new(executable);
            command.arg(entry);
            command
        }
        WorkerLanguage::Javascript => {
            unreachable!("mobile JavaScript runtime was rejected before local worker selection");
        }
    };

    apply_worker_environment(&mut command, &request.language, &runtime_dir, desktop_role)?;

    let browser_engine = match request.browser_engine {
        BrowserEngine::Selenium => "selenium",
        BrowserEngine::Playwright => "playwright",
        BrowserEngine::Puppeteer => "puppeteer",
        BrowserEngine::Webview => {
            unreachable!("mobile WebView runtime was rejected before local worker selection");
        }
    };

    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("TKDA_RUN_ID", run_id.to_string())
        .env("TKDA_TASK_ID", &request.task_id)
        .env("TKDA_BROWSER_ENGINE", browser_engine)
        .env(
            "TKDA_HEADLESS",
            if request.execution_mode.is_headless() {
                "true"
            } else {
                "false"
            },
        );

    match command.spawn() {
        Ok(child) => Ok((child, runtime_dir)),
        Err(error) => {
            let _ = fs::remove_dir_all(&runtime_dir);
            Err(error)
        }
    }
}


struct WorkerEventReader<R> {
    frames: FramedRead<R, LinesCodec>,
    failed: bool,
}

impl<R> WorkerEventReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R) -> Self {
        return Self {
            frames: FramedRead::new(
                reader,
                LinesCodec::new_with_max_length(MAX_WORKER_EVENT_LINE_BYTES),
            ),
            failed: false,
        };
    }

    async fn next_event(&mut self) -> Result<Option<WorkerEvent>, String> {
        if self.failed {
            return Ok(None);
        }

        let Some(frame) = self.frames.next().await else {
            return Ok(None);
        };

        match frame {
            Ok(line) => match serde_json::from_str::<WorkerEvent>(&line) {
                Ok(event) => Ok(Some(event)),
                Err(error) => {
                    self.failed = true;
                    Err(format!("worker emitted invalid event JSON: {error}"))
                }
            },
            Err(error) => {
                self.failed = true;
                Err(format!(
                    "worker event stream violated the {MAX_WORKER_EVENT_LINE_BYTES}-byte line policy: {error}"
                ))
            }
        }
    }
}

#[derive(Clone, Debug)]
enum LocalTerminalSignal {
    Completed,
    Failed { retryable: bool, error: String },
}

async fn run_local_attempt(
    control: Arc<RunControl>,
    request: CreateRunRequest,
    mut child: Child,
    attempt_budget: Duration,
    lease_epoch: u64,
) -> AttemptOutcome {
    {
        let mut snapshot = control.snapshot.write().await;
        snapshot.pid = child.id();
        snapshot.status = RunStatus::Starting;
        snapshot.last_message = Some("worker spawned; awaiting ready handshake".to_string());
    }

    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return AttemptOutcome::Failed {
                retryable: true,
                error: "worker stdin unavailable".to_string(),
            };
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return AttemptOutcome::Failed {
                retryable: true,
                error: "worker stdout unavailable".to_string(),
            };
        }
    };

    let mut commands = control.command_tx.subscribe();
    let writer = tokio::spawn(async move {
        while let Ok(command) = commands.recv().await {
            let mut line = match serde_json::to_vec(&command) {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(%error, "failed to encode worker command");
                    continue;
                }
            };
            line.push(b'\n');
            if stdin.write_all(&line).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });

    let reader_control = control.clone();
    let terminal_signal = Arc::new(std::sync::Mutex::new(None::<LocalTerminalSignal>));
    let reader_terminal_signal = terminal_signal.clone();
    let (protocol_violation_tx, protocol_violation_rx) = oneshot::channel::<String>();
    let mut reader = tokio::spawn(async move {
        let mut events = WorkerEventReader::new(stdout);
        let mut protocol_violation_tx = Some(protocol_violation_tx);
        loop {
            match events.next_event().await {
                Ok(Some(event)) => {
                    if let Err(error) = reader_control.validate_event_epoch(&event).await {
                        if let Some(sender) = protocol_violation_tx.take() {
                            let _ = sender.send(error);
                        }
                        break;
                    }
                    let terminal = match &event {
                        WorkerEvent::Completed { .. } => Some(LocalTerminalSignal::Completed),
                        WorkerEvent::Failed { retryable, error, .. } => {
                            Some(LocalTerminalSignal::Failed {
                                retryable: *retryable,
                                error: truncate_message(error),
                            })
                        }
                        _ => None,
                    };
                    if let Some(terminal) = terminal {
                        let mut slot = reader_terminal_signal
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if slot.is_some() {
                            if let Some(sender) = protocol_violation_tx.take() {
                                let _ = sender.send(
                                    "worker emitted more than one terminal event".to_string(),
                                );
                            }
                            break;
                        }
                        *slot = Some(terminal);
                    }

                    reader_control.record_worker_event(&event).await;
                    let _ = reader_control.event_tx.send(event);
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        max_bytes = MAX_WORKER_EVENT_LINE_BYTES,
                        "worker event stream violated framing or JSON policy"
                    );
                    if let Some(sender) = protocol_violation_tx.take() {
                        let _ = sender.send(error);
                    }
                    break;
                }
            }
        }
    });

    let run_id = control.snapshot.read().await.run_id;
    let _ = control
        .command_tx
        .send(start_command(run_id, lease_epoch, &request));

    let outcome = tokio::select! {
        error = async {
            match protocol_violation_rx.await {
                Ok(error) => error,
                Err(_) => pending::<String>().await,
            }
        } => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            AttemptOutcome::Failed {
                retryable: false,
                error,
            }
        }
        _ = control.cancel.cancelled() => {
            let _ = control.command_tx.send(WorkerCommand::Cancel {
                reason: "supervisor cancellation".to_string(),
            });
            let _ = child.kill().await;
            let _ = child.wait().await;
            AttemptOutcome::Cancelled
        }
        result = timeout(attempt_budget, child.wait()) => {
            match result {
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    AttemptOutcome::TimedOut
                }
                Ok(Err(error)) => AttemptOutcome::Failed {
                    retryable: true,
                    error: format!("worker wait failed: {error}"),
                },
                Ok(Ok(status)) => {
                    let drained = timeout(Duration::from_secs(2), &mut reader).await;
                    if drained.is_err() {
                        AttemptOutcome::Failed {
                            retryable: false,
                            error: "worker exited but its event stream did not close within 2s".to_string(),
                        }
                    } else {
                        let terminal = terminal_signal
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone();
                        match terminal {
                            Some(LocalTerminalSignal::Completed) if status.success() => {
                                AttemptOutcome::Succeeded
                            }
                            Some(LocalTerminalSignal::Completed) => AttemptOutcome::Failed {
                                retryable: false,
                                error: format!(
                                    "worker emitted completed but then exited unsuccessfully with {status}"
                                ),
                            },
                            Some(LocalTerminalSignal::Failed { retryable, error }) => {
                                AttemptOutcome::Failed { retryable, error }
                            }
                            None if status.success() => AttemptOutcome::Failed {
                                retryable: false,
                                error: "worker exited successfully without a completed event".to_string(),
                            },
                            None => AttemptOutcome::Failed {
                                retryable: true,
                                error: format!("worker exited with {status} before a terminal event"),
                            },
                        }
                    }
                }
            }
        }
    };

    writer.abort();
    reader.abort();

    {
        let mut snapshot = control.snapshot.write().await;
        snapshot.pid = None;
    }

    return outcome;
}

fn start_command(run_id: Uuid, lease_epoch: u64, request: &CreateRunRequest) -> WorkerCommand {
    return WorkerCommand::Start {
        run_id,
        lease_epoch,
        task_id: request.task_id.clone(),
        prompt: request.prompt.clone(),
        browser_engine: request.browser_engine.clone(),
        execution_mode: request.execution_mode.clone(),
        source_revision: request.source_revision.clone(),
        ai: request.ai.clone(),
    };
}

fn truncate_message(message: &str) -> String {
    return message.chars().take(MAX_STATUS_MESSAGE_CHARS).collect();
}

fn unix_time_ms() -> u64 {
    return SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
}

async fn set_status(control: &RunControl, status: RunStatus, message: Option<&str>) {
    let mut snapshot = control.snapshot.write().await;
    snapshot.status = status;
    snapshot.pid = None;
    snapshot.last_message = message.map(truncate_message);
}

fn remote_cleanup_reason(outcome: &AttemptOutcome) -> Option<&'static str> {
    return match outcome {
        AttemptOutcome::Succeeded => None,
        AttemptOutcome::Cancelled => Some("supervisor cancellation"),
        AttemptOutcome::TimedOut => Some("global run timeout"),
        AttemptOutcome::Failed { .. } => Some("remote attempt fenced before reassignment"),
    };
}

enum AttemptOutcome {
    Succeeded,
    Cancelled,
    TimedOut,
    Failed { error: String, retryable: bool },
}


#[cfg(test)]
mod lease_epoch_tests {
    use super::*;

    fn control_with_epoch(lease_epoch: u64) -> RunControl {
        let (command_tx, _) = broadcast::channel(8);
        let (event_tx, _) = broadcast::channel(8);
        RunControl {
            snapshot: RwLock::new(RunSnapshot {
                run_id: Uuid::new_v4(),
                task_id: "task-1".to_owned(),
                language: WorkerLanguage::Typescript,
                browser_engine: BrowserEngine::Playwright,
                execution_target: ExecutionTarget::Local,
                placement_preference: PlacementPreference::Cloud,
                execution_mode: ExecutionMode::Headless,
                execution_location: Some(ExecutionLocation::Cloud),
                preferred_agent_id: None,
                executor_id: None,
                status: RunStatus::Running,
                attempt: 2,
                max_retries: 2,
                timeout_secs: MIN_RUN_SECONDS,
                pid: None,
                execution_id: None,
                lease_epoch,
                last_heartbeat_unix_ms: None,
                last_message: None,
            }),
            command_tx,
            event_tx,
            cancel: CancellationToken::new(),
            remote_event_token: RwLock::new(None),
            last_heartbeat: RwLock::new(None),
        }
    }

    #[tokio::test]
    async fn stale_and_zero_epoch_events_are_rejected() {
        let control = control_with_epoch(2);
        let stale = WorkerEvent::Ready {
            lease_epoch: 1,
            transport: Some("stdio".to_owned()),
        };
        let current = WorkerEvent::Ready {
            lease_epoch: 2,
            transport: Some("stdio".to_owned()),
        };
        let zero = WorkerEvent::Heartbeat { lease_epoch: 0 };

        assert!(control.validate_event_epoch(&stale).await.is_err());
        assert!(control.validate_event_epoch(&zero).await.is_err());
        assert!(control.validate_event_epoch(&current).await.is_ok());
    }

    #[test]
    fn start_command_carries_the_attempt_epoch() {
        let request = CreateRunRequest {
            task_id: "task-1".to_owned(),
            prompt: "open example.com".to_owned(),
            language: WorkerLanguage::Typescript,
            browser_engine: BrowserEngine::Playwright,
            source_revision: None,
            execution_target: ExecutionTarget::Local,
            placement_preference: PlacementPreference::Cloud,
            execution_mode: ExecutionMode::Headless,
            preferred_agent_id: None,
            timeout_secs: MIN_RUN_SECONDS,
            max_retries: 0,
            ai: Default::default(),
        };
        match start_command(Uuid::new_v4(), 9, &request) {
            WorkerCommand::Start { lease_epoch, .. } => assert_eq!(lease_epoch, 9),
            _ => panic!("expected start command"),
        }
    }
}

#[cfg(test)]
mod remote_event_token_tests {
    use super::constant_time_token_eq;

    #[test]
    fn remote_event_token_comparison_rejects_length_and_value_mismatch() {
        let token = b"abcdefghijklmnopqrstuvwxyz012345";
        assert!(constant_time_token_eq(token, token));
        assert!(!constant_time_token_eq(token, b"abcdefghijklmnopqrstuvwxyz012346"));
        assert!(!constant_time_token_eq(token, b"abcdefghijklmnopqrstuvwxyz012345extra"));
        assert!(!constant_time_token_eq(token, b"abcdefghijklmnopqrstuvwxyz01234"));
    }
}
