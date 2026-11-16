use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLanguage {
    Typescript,
    Javascript,
    Rust,
    Go,
    Python,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEngine {
    Selenium,
    Playwright,
    Puppeteer,
    Webview,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Headless,
    Headed,
}

impl ExecutionMode {
    pub fn is_headless(&self) -> bool {
        return matches!(self, Self::Headless);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTarget {
    #[default]
    Local,
    Scintilla,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlacementPreference {
    #[default]
    Auto,
    Cloud,
    Desktop,
    Mobile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLocation {
    Cloud,
    Desktop,
    Mobile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiPolicy {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_ai_steps")]
    pub max_planning_steps: u32,
    #[serde(deserialize_with = "deserialize_ai_replans")]
    pub max_replans: u32,
}

impl Default for AiPolicy {
    fn default() -> Self {
        return Self {
            enabled: false,
            max_planning_steps: default_ai_steps(),
            max_replans: default_ai_replans(),
        };
    }
}

fn default_ai_steps() -> u32 {
    return 16;
}

fn default_ai_replans() -> u32 {
    return 3;
}

fn deserialize_ai_steps<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if !(1..=64).contains(&value) {
        return Err(D::Error::custom(
            "max_planning_steps must be between 1 and 64",
        ));
    }
    return Ok(value);
}

fn deserialize_ai_replans<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if value > 8 {
        return Err(D::Error::custom("max_replans must be between 0 and 8"));
    }
    return Ok(value);
}

fn deserialize_timeout_secs<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if !(1200..=7200).contains(&value) {
        return Err(D::Error::custom(
            "timeout_secs must be between 1200 and 7200",
        ));
    }
    return Ok(value);
}

fn deserialize_max_retries<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if value > 10 {
        return Err(D::Error::custom("max_retries must be between 0 and 10"));
    }
    return Ok(value);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRunRequest {
    pub task_id: String,
    pub prompt: String,
    pub language: WorkerLanguage,
    pub browser_engine: BrowserEngine,
    pub source_revision: Option<String>,
    #[serde(default)]
    pub execution_target: ExecutionTarget,
    #[serde(default)]
    pub placement_preference: PlacementPreference,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub preferred_agent_id: Option<String>,
    #[serde(deserialize_with = "deserialize_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(deserialize_with = "deserialize_max_retries")]
    pub max_retries: u32,
    pub ai: AiPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Starting,
    Running,
    Retrying,
    Cancelling,
    Cancelled,
    Succeeded,
    Failed,
    TimedOut,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        return matches!(
            self,
            Self::Cancelled | Self::Succeeded | Self::Failed | Self::TimedOut
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: Uuid,
    pub task_id: String,
    pub language: WorkerLanguage,
    pub browser_engine: BrowserEngine,
    pub execution_target: ExecutionTarget,
    pub placement_preference: PlacementPreference,
    pub execution_mode: ExecutionMode,
    pub execution_location: Option<ExecutionLocation>,
    pub preferred_agent_id: Option<String>,
    pub executor_id: Option<String>,
    pub status: RunStatus,
    pub attempt: u32,
    pub max_retries: u32,
    pub timeout_secs: u64,
    pub pid: Option<u32>,
    pub execution_id: Option<String>,
    pub lease_epoch: u64,
    pub last_heartbeat_unix_ms: Option<u64>,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerCommand {
    Start {
        run_id: Uuid,
        lease_epoch: u64,
        task_id: String,
        prompt: String,
        browser_engine: BrowserEngine,
        #[serde(default)]
        execution_mode: ExecutionMode,
        source_revision: Option<String>,
        ai: AiPolicy,
    },
    Driver {
        request_id: Uuid,
        method: String,
        path: String,
        body: serde_json::Value,
    },
    Replan {
        reason: String,
    },
    Cancel {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready {
        lease_epoch: u64,
        transport: Option<String>,
    },
    Heartbeat {
        lease_epoch: u64,
    },
    Log {
        lease_epoch: u64,
        level: String,
        message: String,
    },
    DriverResponse {
        lease_epoch: u64,
        request_id: Uuid,
        status: u16,
        body: serde_json::Value,
    },
    Checkpoint {
        lease_epoch: u64,
        sequence: u64,
        name: String,
        data: serde_json::Value,
    },
    NeedsReplan {
        lease_epoch: u64,
        reason: String,
    },
    Completed {
        lease_epoch: u64,
        output: serde_json::Value,
    },
    Failed {
        lease_epoch: u64,
        retryable: bool,
        error: String,
    },
}

impl WorkerEvent {
    pub fn lease_epoch(&self) -> u64 {
        match self {
            Self::Ready { lease_epoch, .. }
            | Self::Heartbeat { lease_epoch }
            | Self::Log { lease_epoch, .. }
            | Self::DriverResponse { lease_epoch, .. }
            | Self::Checkpoint { lease_epoch, .. }
            | Self::NeedsReplan { lease_epoch, .. }
            | Self::Completed { lease_epoch, .. }
            | Self::Failed { lease_epoch, .. } => *lease_epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> serde_json::Value {
        return serde_json::json!({
            "task_id": "task",
            "prompt": "prompt",
            "language": "typescript",
            "browser_engine": "playwright",
            "source_revision": null,
            "timeout_secs": 1800,
            "max_retries": 2,
            "ai": {"enabled": false, "max_planning_steps": 16, "max_replans": 3}
        });
    }

    #[test]
    fn execution_axes_default_independently() {
        let request: CreateRunRequest =
            serde_json::from_value(base_request()).expect("run request");
        assert_eq!(request.execution_target, ExecutionTarget::Local);
        assert_eq!(request.placement_preference, PlacementPreference::Auto);
        assert_eq!(request.execution_mode, ExecutionMode::Headless);
    }

    #[test]
    fn required_run_policy_fields_may_not_be_omitted() {
        for field in ["timeout_secs", "max_retries", "ai"] {
            let mut value = base_request();
            value.as_object_mut().expect("object").remove(field);
            assert!(
                serde_json::from_value::<CreateRunRequest>(value).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn run_policy_ranges_fail_during_deserialization() {
        let mut timeout = base_request();
        timeout["timeout_secs"] = serde_json::json!(1199);
        assert!(serde_json::from_value::<CreateRunRequest>(timeout).is_err());

        let mut retries = base_request();
        retries["max_retries"] = serde_json::json!(11);
        assert!(serde_json::from_value::<CreateRunRequest>(retries).is_err());

        let mut planning = base_request();
        planning["ai"]["max_planning_steps"] = serde_json::json!(0);
        assert!(serde_json::from_value::<CreateRunRequest>(planning).is_err());
    }

    #[test]
    fn mobile_runtime_vocabulary_deserializes() {
        let request: CreateRunRequest = serde_json::from_value(serde_json::json!({
            "task_id": "mobile-task",
            "prompt": "open example.com",
            "language": "javascript",
            "browser_engine": "webview",
            "execution_target": "local",
            "placement_preference": "mobile",
            "execution_mode": "headed",
            "timeout_secs": 1800,
            "max_retries": 2,
            "ai": {"enabled": false, "max_planning_steps": 16, "max_replans": 3}
        }))
        .expect("mobile run request");
        assert_eq!(request.language, WorkerLanguage::Javascript);
        assert_eq!(request.browser_engine, BrowserEngine::Webview);
        assert_eq!(request.placement_preference, PlacementPreference::Mobile);
    }

    #[test]
    fn worker_events_require_positive_lease_epoch_shape() {
        let ready: WorkerEvent = serde_json::from_value(serde_json::json!({
            "type": "ready",
            "lease_epoch": 7,
            "transport": "stdio"
        }))
        .expect("fenced ready event");
        assert_eq!(ready.lease_epoch(), 7);

        assert!(
            serde_json::from_value::<WorkerEvent>(serde_json::json!({
                "type": "ready",
                "transport": "stdio"
            }))
            .is_err()
        );
    }

    #[test]
    fn worker_start_defaults_to_headless_execution_mode() {
        let command: WorkerCommand = serde_json::from_value(serde_json::json!({
            "type": "start",
            "run_id": Uuid::new_v4(),
            "lease_epoch": 1,
            "task_id": "task",
            "prompt": "prompt",
            "browser_engine": "playwright",
            "source_revision": null,
            "ai": {"enabled": false, "max_planning_steps": 16, "max_replans": 3}
        }))
        .expect("worker start");
        match command {
            WorkerCommand::Start { execution_mode, .. } => {
                assert_eq!(execution_mode, ExecutionMode::Headless);
            }
            _ => panic!("wrong worker command"),
        }
    }
}
