use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use uuid::Uuid;

use crate::protocol::{
    BrowserEngine, CreateRunRequest, ExecutionMode, PlacementPreference, WorkerLanguage,
};

const PROTOCOL_VERSION: u32 = 1;
const HELLO_TIMEOUT_SECONDS: u64 = 10;
const LEASE_TIMEOUT_SECONDS: u64 = 30;
const RPC_TIMEOUT_SECONDS: u64 = 125;
const HEARTBEAT_STALE_MS: u64 = 45_000;
const MAX_ID_CHARS: usize = 256;
const MAX_AGENT_MESSAGE_BYTES: usize = 1_048_576;
const MAX_VERSION_CHARS: usize = 128;
const MAX_PLATFORM_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    Desktop,
    Mobile,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCapabilities {
    pub headed: bool,
    pub headless: bool,
    pub engines: Vec<String>,
    pub languages: Vec<String>,
    pub max_concurrency: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentMessage {
    Hello {
        protocol_version: u32,
        agent_id: String,
        device_class: DeviceClass,
        version: String,
        os: String,
        arch: String,
        capabilities: AgentCapabilities,
    },
    Heartbeat {
        agent_id: String,
    },
    LeaseAccepted {
        lease_id: String,
        run_id: Option<String>,
    },
    LeaseRejected {
        lease_id: String,
        reason: String,
    },
    RpcResult {
        request_id: String,
        status: u16,
        body: Value,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ping,
    Lease {
        lease_id: String,
        run: Value,
    },
    Rpc {
        request_id: String,
        method: String,
        path: String,
        body: Value,
    },
}

#[derive(Clone)]
struct AgentConnection {
    connection_id: Uuid,
    device_class: DeviceClass,
    capabilities: AgentCapabilities,
    tx: mpsc::Sender<ServerMessage>,
    active: Arc<AtomicU32>,
    heartbeat_unix_ms: Arc<AtomicU64>,
}

struct PendingLease {
    agent_id: String,
    tx: oneshot::Sender<Result<String, String>>,
}

struct PendingRpc {
    agent_id: String,
    tx: oneshot::Sender<Result<RpcResponse, String>>,
}

#[derive(Debug)]
pub struct RpcResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Debug)]
pub struct DeviceLease {
    pub lease_id: String,
    pub agent_id: String,
    pub device_class: DeviceClass,
    pub local_run_id: String,
}

#[derive(Clone, Default)]
pub struct AgentRegistry {
    agents: Arc<DashMap<String, AgentConnection>>,
    pending_leases: Arc<DashMap<String, PendingLease>>,
    pending_rpcs: Arc<DashMap<String, PendingRpc>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        return Self::default();
    }

    pub fn has_matching_agent(&self, request: &CreateRunRequest) -> bool {
        return self.select_agent(request).is_some();
    }

    pub async fn lease(&self, request: &CreateRunRequest) -> Result<DeviceLease, String> {
        let run = serde_json::to_value(request)
            .map_err(|error| format!("failed to encode device run: {error}"))?;
        let (agent_id, agent) = self
            .reserve_agent(request)
            .ok_or_else(|| "no connected device agent has capacity for this run".to_string())?;
        let lease_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending_leases.insert(
            lease_id.clone(),
            PendingLease {
                agent_id: agent_id.clone(),
                tx,
            },
        );

        if agent
            .tx
            .send(ServerMessage::Lease {
                lease_id: lease_id.clone(),
                run,
            })
            .await
            .is_err()
        {
            self.pending_leases.remove(&lease_id);
            decrement(&agent.active);
            return Err("device agent command channel closed".to_string());
        }

        let result = timeout(Duration::from_secs(LEASE_TIMEOUT_SECONDS), rx)
            .await
            .map_err(|_| "device lease acknowledgement timed out".to_string())
            .and_then(|value| value.map_err(|_| "device lease acknowledgement dropped".to_string()))
            .and_then(|value| value);

        self.pending_leases.remove(&lease_id);
        match result {
            Ok(local_run_id) => {
                return Ok(DeviceLease {
                    lease_id,
                    agent_id,
                    device_class: agent.device_class,
                    local_run_id,
                });
            }
            Err(error) => {
                decrement(&agent.active);
                return Err(error);
            }
        }
    }

    pub async fn rpc(
        &self,
        agent_id: &str,
        method: &str,
        path: &str,
        body: Value,
    ) -> Result<RpcResponse, String> {
        let agent = self
            .agents
            .get(agent_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| "device agent is disconnected".to_string())?;
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending_rpcs.insert(
            request_id.clone(),
            PendingRpc {
                agent_id: agent_id.to_string(),
                tx,
            },
        );

        if agent
            .tx
            .send(ServerMessage::Rpc {
                request_id: request_id.clone(),
                method: method.to_string(),
                path: path.to_string(),
                body,
            })
            .await
            .is_err()
        {
            self.pending_rpcs.remove(&request_id);
            return Err("device agent command channel closed".to_string());
        }

        let result = timeout(Duration::from_secs(RPC_TIMEOUT_SECONDS), rx)
            .await
            .map_err(|_| "device agent RPC timed out".to_string())
            .and_then(|value| value.map_err(|_| "device agent RPC dropped".to_string()))
            .and_then(|value| value);
        self.pending_rpcs.remove(&request_id);
        return result;
    }

    pub fn release(&self, lease: &DeviceLease) {
        if let Some(agent) = self.agents.get(&lease.agent_id) {
            decrement(&agent.active);
        }
    }

    pub async fn websocket(&self, mut socket: WebSocket, presented_token: String) {
        let hello = match timeout(Duration::from_secs(HELLO_TIMEOUT_SECONDS), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_AGENT_MESSAGE_BYTES => {
                serde_json::from_str::<AgentMessage>(text.as_ref()).ok()
            }
            _ => None,
        };

        let (agent_id, device_class, capabilities, version, os, arch) = match hello {
            Some(AgentMessage::Hello {
                protocol_version,
                agent_id,
                device_class,
                version,
                os,
                arch,
                capabilities,
            }) if protocol_version == PROTOCOL_VERSION
                && valid_id(&agent_id)
                && valid_label(&version, MAX_VERSION_CHARS)
                && valid_label(&os, MAX_PLATFORM_CHARS)
                && valid_label(&arch, MAX_PLATFORM_CHARS)
                && valid_capabilities(device_class, &capabilities) =>
            {
                (agent_id, device_class, capabilities, version, os, arch)
            }
            _ => {
                let _ = socket.close().await;
                return;
            }
        };

        if !agent_token_authorized(&agent_id, &presented_token) {
            tracing::warn!(%agent_id, ?device_class, "device agent authentication rejected");
            let _ = socket.close().await;
            return;
        }

        let connection_id = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(512);
        self.agents.insert(
            agent_id.clone(),
            AgentConnection {
                connection_id,
                device_class,
                capabilities,
                tx,
                active: Arc::new(AtomicU32::new(0)),
                heartbeat_unix_ms: Arc::new(AtomicU64::new(unix_time_ms())),
            },
        );

        tracing::info!(
            %agent_id,
            %connection_id,
            ?device_class,
            %version,
            %os,
            %arch,
            "device agent connected"
        );

        let (mut sender, mut receiver) = socket.split();
        let mut ping = interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    let Ok(body) = serde_json::to_string(&ServerMessage::Ping) else {
                        break;
                    };
                    if body.len() > MAX_AGENT_MESSAGE_BYTES
                        || sender.send(Message::Text(body.into())).await.is_err()
                    {
                        break;
                    }
                }
                outbound = rx.recv() => {
                    let Some(outbound) = outbound else {
                        break;
                    };
                    let Ok(body) = serde_json::to_string(&outbound) else {
                        continue;
                    };
                    if body.len() > MAX_AGENT_MESSAGE_BYTES
                        || sender.send(Message::Text(body.into())).await.is_err()
                    {
                        break;
                    }
                }
                inbound = receiver.next() => {
                    match inbound {
                        Some(Ok(Message::Text(text))) => {
                            if text.len() > MAX_AGENT_MESSAGE_BYTES {
                                break;
                            }
                            let Ok(message) = serde_json::from_str::<AgentMessage>(text.as_ref()) else {
                                break;
                            };
                            if !self.handle_agent_message(&agent_id, connection_id, message).await {
                                break;
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {
                            break;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if sender.send(Message::Pong(payload)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        self.unregister(&agent_id, connection_id);
    }

    async fn handle_agent_message(
        &self,
        connected_agent_id: &str,
        connection_id: Uuid,
        message: AgentMessage,
    ) -> bool {
        let current = self
            .agents
            .get(connected_agent_id)
            .is_some_and(|entry| entry.connection_id == connection_id);
        if !current {
            return false;
        }

        match message {
            AgentMessage::Heartbeat { agent_id } => {
                if agent_id != connected_agent_id {
                    return false;
                }
                if let Some(agent) = self.agents.get(connected_agent_id) {
                    agent
                        .heartbeat_unix_ms
                        .store(unix_time_ms(), Ordering::Release);
                }
            }
            AgentMessage::LeaseAccepted { lease_id, run_id } => {
                if !valid_id(&lease_id) {
                    return false;
                }
                let Some((_, pending)) = self.pending_leases.remove(&lease_id) else {
                    return true;
                };
                if pending.agent_id != connected_agent_id {
                    let _ = pending
                        .tx
                        .send(Err("device lease agent mismatch".to_string()));
                    return false;
                }
                let result = run_id
                    .filter(|value| valid_id(value))
                    .ok_or_else(|| "device agent lease omitted a valid run_id".to_string());
                let _ = pending.tx.send(result);
            }
            AgentMessage::LeaseRejected { lease_id, reason } => {
                if !valid_id(&lease_id) {
                    return false;
                }
                if let Some((_, pending)) = self.pending_leases.remove(&lease_id) {
                    if pending.agent_id != connected_agent_id {
                        let _ = pending
                            .tx
                            .send(Err("device lease agent mismatch".to_string()));
                        return false;
                    }
                    let _ = pending.tx.send(Err(truncate(&reason, 20_000)));
                }
            }
            AgentMessage::RpcResult {
                request_id,
                status,
                body,
            } => {
                if !valid_id(&request_id) || !(100..=599).contains(&status) {
                    return false;
                }
                if let Some((_, pending)) = self.pending_rpcs.remove(&request_id) {
                    if pending.agent_id != connected_agent_id {
                        let _ = pending
                            .tx
                            .send(Err("device RPC agent mismatch".to_string()));
                        return false;
                    }
                    let _ = pending.tx.send(Ok(RpcResponse { status, body }));
                }
            }
            AgentMessage::Hello { .. } => {
                return false;
            }
        }
        return true;
    }

    fn unregister(&self, agent_id: &str, connection_id: Uuid) {
        let is_current = self
            .agents
            .get(agent_id)
            .is_some_and(|entry| entry.connection_id == connection_id);
        if !is_current {
            return;
        }
        self.agents.remove(agent_id);

        let lease_ids: Vec<String> = self
            .pending_leases
            .iter()
            .filter_map(|entry| (entry.value().agent_id == agent_id).then(|| entry.key().clone()))
            .collect();
        for lease_id in lease_ids {
            if let Some((_, pending)) = self.pending_leases.remove(&lease_id) {
                let _ = pending
                    .tx
                    .send(Err("device agent disconnected".to_string()));
            }
        }

        let request_ids: Vec<String> = self
            .pending_rpcs
            .iter()
            .filter_map(|entry| (entry.value().agent_id == agent_id).then(|| entry.key().clone()))
            .collect();
        for request_id in request_ids {
            if let Some((_, pending)) = self.pending_rpcs.remove(&request_id) {
                let _ = pending
                    .tx
                    .send(Err("device agent disconnected".to_string()));
            }
        }

        tracing::info!(%agent_id, %connection_id, "device agent disconnected");
    }

    fn reserve_agent(&self, request: &CreateRunRequest) -> Option<(String, AgentConnection)> {
        let now = unix_time_ms();
        let preferred = request.preferred_agent_id.as_deref();
        let mut matches = self
            .agents
            .iter()
            .filter_map(|entry| {
                if preferred.is_some_and(|value| value != entry.key().as_str()) {
                    return None;
                }
                let agent = entry.value();
                let heartbeat = agent.heartbeat_unix_ms.load(Ordering::Acquire);
                if now.saturating_sub(heartbeat) > HEARTBEAT_STALE_MS {
                    return None;
                }
                if !matches_request(agent.device_class, &agent.capabilities, request) {
                    return None;
                }
                let active = agent.active.load(Ordering::Acquire);
                if active >= agent.capabilities.max_concurrency {
                    return None;
                }
                return Some((entry.key().clone(), agent.clone(), active));
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(_, _, active)| *active);

        for (agent_id, agent, _) in matches {
            if try_reserve(&agent.active, agent.capabilities.max_concurrency) {
                return Some((agent_id, agent));
            }
        }
        return None;
    }

    fn select_agent(&self, request: &CreateRunRequest) -> Option<(String, AgentConnection)> {
        let now = unix_time_ms();
        let preferred = request.preferred_agent_id.as_deref();
        let mut matches = self
            .agents
            .iter()
            .filter_map(|entry| {
                if preferred.is_some_and(|value| value != entry.key().as_str()) {
                    return None;
                }
                let agent = entry.value();
                let heartbeat = agent.heartbeat_unix_ms.load(Ordering::Acquire);
                if now.saturating_sub(heartbeat) > HEARTBEAT_STALE_MS {
                    return None;
                }
                if !matches_request(agent.device_class, &agent.capabilities, request) {
                    return None;
                }
                let active = agent.active.load(Ordering::Acquire);
                if active >= agent.capabilities.max_concurrency {
                    return None;
                }
                return Some((entry.key().clone(), agent.clone(), active));
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(_, _, active)| *active);
        return matches
            .into_iter()
            .next()
            .map(|(agent_id, agent, _)| (agent_id, agent));
    }
}

fn matches_request(
    device_class: DeviceClass,
    capabilities: &AgentCapabilities,
    request: &CreateRunRequest,
) -> bool {
    let placement_matches = match request.placement_preference {
        PlacementPreference::Auto => device_class == DeviceClass::Desktop,
        PlacementPreference::Desktop => device_class == DeviceClass::Desktop,
        PlacementPreference::Mobile => device_class == DeviceClass::Mobile,
        PlacementPreference::Cloud => false,
    };
    if !placement_matches {
        return false;
    }

    let engine = match request.browser_engine {
        BrowserEngine::Selenium => "selenium",
        BrowserEngine::Playwright => "playwright",
        BrowserEngine::Puppeteer => "puppeteer",
        BrowserEngine::Webview => "webview",
    };
    let language = match request.language {
        WorkerLanguage::Typescript => "typescript",
        WorkerLanguage::Javascript => "javascript",
        WorkerLanguage::Rust => "rust",
        WorkerLanguage::Go => "go",
        WorkerLanguage::Python => "python",
    };
    let mode = match request.execution_mode {
        ExecutionMode::Headless => capabilities.headless,
        ExecutionMode::Headed => capabilities.headed,
    };
    return mode
        && capabilities.engines.iter().any(|value| value == engine)
        && capabilities.languages.iter().any(|value| value == language);
}

fn valid_capabilities(device_class: DeviceClass, capabilities: &AgentCapabilities) -> bool {
    if capabilities.max_concurrency == 0
        || capabilities.max_concurrency > 64
        || capabilities.engines.is_empty()
        || capabilities.engines.len() > 16
        || capabilities.languages.is_empty()
        || capabilities.languages.len() > 16
    {
        return false;
    }

    let engine_set = capabilities
        .engines
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let language_set = capabilities
        .languages
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if engine_set.len() != capabilities.engines.len()
        || language_set.len() != capabilities.languages.len()
    {
        return false;
    }

    return match device_class {
        DeviceClass::Desktop => {
            let allowed_engines = ["selenium", "playwright", "puppeteer"];
            let allowed_languages = ["typescript", "rust", "go", "python"];
            capabilities
                .engines
                .iter()
                .all(|value| allowed_engines.contains(&value.as_str()))
                && capabilities
                    .languages
                    .iter()
                    .all(|value| allowed_languages.contains(&value.as_str()))
        }
        DeviceClass::Mobile => {
            capabilities.max_concurrency == 1
                && capabilities.headed
                && !capabilities.headless
                && capabilities.engines == ["webview"]
                && capabilities.languages == ["javascript"]
        }
    };
}

fn valid_id(value: &str) -> bool {
    return !value.is_empty()
        && value.chars().count() <= MAX_ID_CHARS
        && !value.chars().any(char::is_whitespace);
}

fn valid_label(value: &str, max_chars: usize) -> bool {
    return !value.is_empty()
        && value.chars().count() <= max_chars
        && !value.chars().any(|character| character.is_control());
}

fn try_reserve(value: &AtomicU32, max: u32) -> bool {
    let mut current = value.load(Ordering::Acquire);
    loop {
        if current >= max {
            return false;
        }
        match value.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                return true;
            }
            Err(observed) => {
                current = observed;
            }
        }
    }
}

fn decrement(value: &AtomicU32) {
    let _ = value.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        return Some(current.saturating_sub(1));
    });
}

fn unix_time_ms() -> u64 {
    return SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
}

fn agent_token_authorized(agent_id: &str, presented: &str) -> bool {
    if let Ok(raw) = std::env::var("TKDA_AGENT_TOKENS_JSON") {
        let Ok(tokens) = serde_json::from_str::<std::collections::HashMap<String, String>>(&raw)
        else {
            tracing::error!("TKDA_AGENT_TOKENS_JSON is not a valid JSON object");
            return false;
        };
        let Some(expected) = tokens.get(agent_id) else {
            return false;
        };
        return valid_secret(expected)
            && constant_time_eq(expected.as_bytes(), presented.as_bytes());
    }

    let Ok(expected) = std::env::var("TKDA_AGENT_TOKEN") else {
        tracing::error!("device agent authentication is not configured");
        return false;
    };
    return valid_secret(&expected) && constant_time_eq(expected.as_bytes(), presented.as_bytes());
}

fn valid_secret(value: &str) -> bool {
    return value.len() >= 32 && value.len() <= 16_384 && !value.chars().any(char::is_whitespace);
}

fn constant_time_eq(expected: &[u8], presented: &[u8]) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in expected.iter().zip(presented.iter()) {
        diff |= a ^ b;
    }
    return diff == 0;
}

fn truncate(value: &str, max_chars: usize) -> String {
    return value.chars().take(max_chars).collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AiPolicy, ExecutionTarget};

    fn desktop_request(mode: ExecutionMode) -> CreateRunRequest {
        return CreateRunRequest {
            task_id: "task".to_string(),
            prompt: "prompt".to_string(),
            language: WorkerLanguage::Typescript,
            browser_engine: BrowserEngine::Playwright,
            source_revision: None,
            execution_target: ExecutionTarget::Local,
            placement_preference: PlacementPreference::Desktop,
            execution_mode: mode,
            preferred_agent_id: None,
            timeout_secs: 1800,
            max_retries: 2,
            ai: AiPolicy::default(),
        };
    }

    fn mobile_request() -> CreateRunRequest {
        return CreateRunRequest {
            task_id: "mobile-task".to_string(),
            prompt: "prompt".to_string(),
            language: WorkerLanguage::Javascript,
            browser_engine: BrowserEngine::Webview,
            source_revision: None,
            execution_target: ExecutionTarget::Local,
            placement_preference: PlacementPreference::Mobile,
            execution_mode: ExecutionMode::Headed,
            preferred_agent_id: None,
            timeout_secs: 1800,
            max_retries: 2,
            ai: AiPolicy::default(),
        };
    }

    fn desktop_caps() -> AgentCapabilities {
        return AgentCapabilities {
            headed: true,
            headless: true,
            engines: vec!["playwright".to_string()],
            languages: vec!["typescript".to_string()],
            max_concurrency: 1,
        };
    }

    fn mobile_caps() -> AgentCapabilities {
        return AgentCapabilities {
            headed: true,
            headless: false,
            engines: vec!["webview".to_string()],
            languages: vec!["javascript".to_string()],
            max_concurrency: 1,
        };
    }

    #[test]
    fn atomic_reservation_never_exceeds_single_slot() {
        let active = Arc::new(AtomicU32::new(0));
        let wins = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(32));
        let mut handles = Vec::new();

        for _ in 0..32 {
            let active = Arc::clone(&active);
            let wins = Arc::clone(&wins);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                if try_reserve(&active, 1) {
                    wins.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }

        for handle in handles {
            handle.join().expect("reservation contender panicked");
        }

        assert_eq!(wins.load(Ordering::Acquire), 1);
        assert_eq!(active.load(Ordering::Acquire), 1);
    }

    #[test]
    fn atomic_reservation_respects_multi_slot_capacity() {
        let active = AtomicU32::new(0);
        assert!(try_reserve(&active, 2));
        assert!(try_reserve(&active, 2));
        assert!(!try_reserve(&active, 2));
        assert_eq!(active.load(Ordering::Acquire), 2);
        decrement(&active);
        assert!(try_reserve(&active, 2));
        assert_eq!(active.load(Ordering::Acquire), 2);
    }

    #[test]
    fn headed_matching_requires_headed_capability() {
        let mut caps = desktop_caps();
        caps.headed = false;
        assert!(matches_request(
            DeviceClass::Desktop,
            &caps,
            &desktop_request(ExecutionMode::Headless)
        ));
        assert!(!matches_request(
            DeviceClass::Desktop,
            &caps,
            &desktop_request(ExecutionMode::Headed)
        ));
    }

    #[test]
    fn mobile_request_matches_only_mobile_webview_agent() {
        assert!(matches_request(
            DeviceClass::Mobile,
            &mobile_caps(),
            &mobile_request()
        ));
        assert!(!matches_request(
            DeviceClass::Desktop,
            &desktop_caps(),
            &mobile_request()
        ));
    }

    #[test]
    fn auto_placement_is_desktop_only_at_registry_boundary() {
        let mut desktop = desktop_request(ExecutionMode::Headless);
        desktop.placement_preference = PlacementPreference::Auto;
        assert!(matches_request(
            DeviceClass::Desktop,
            &desktop_caps(),
            &desktop
        ));

        let mut mobile = mobile_request();
        mobile.placement_preference = PlacementPreference::Auto;
        assert!(!matches_request(
            DeviceClass::Mobile,
            &mobile_caps(),
            &mobile
        ));
    }

    #[test]
    fn capability_validation_is_device_class_specific() {
        assert!(valid_capabilities(DeviceClass::Desktop, &desktop_caps()));
        assert!(valid_capabilities(DeviceClass::Mobile, &mobile_caps()));
        assert!(!valid_capabilities(DeviceClass::Desktop, &mobile_caps()));
        assert!(!valid_capabilities(DeviceClass::Mobile, &desktop_caps()));

        let mut excessive = desktop_caps();
        excessive.max_concurrency = 65;
        assert!(!valid_capabilities(DeviceClass::Desktop, &excessive));
    }

    #[test]
    fn constant_time_compare_matches_equal_values() {
        assert!(constant_time_eq(
            b"abcdefghijklmnopqrstuvwxyz012345",
            b"abcdefghijklmnopqrstuvwxyz012345"
        ));
        assert!(!constant_time_eq(
            b"abcdefghijklmnopqrstuvwxyz012345",
            b"abcdefghijklmnopqrstuvwxyz012346"
        ));
        assert!(!constant_time_eq(b"short", b"shorter"));
    }

    #[test]
    fn validates_agent_secret_shape() {
        assert!(valid_secret("abcdefghijklmnopqrstuvwxyz012345"));
        assert!(!valid_secret("too-short"));
        assert!(!valid_secret("abcdefghijklmnopqrstuvwxyz 012345"));
    }
}
