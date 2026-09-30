use std::{
    fs::{self, File},
    io::Read,
    net::IpAddr,
    path::Path,
    time::Duration,
};

use reqwest::{Client, RequestBuilder, Response, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{
    BrowserEngine, CreateRunRequest, ExecutionMode, WorkerCommand, WorkerLanguage,
};

const REMOTE_HTTP_TIMEOUT_SECONDS: u64 = 30;
const MAX_REMOTE_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_SECRET_FILE_BYTES: u64 = 16 * 1024;

#[derive(Clone)]
pub struct RemoteLauncher {
    http: Client,
    launch_url: Url,
    public_base_url: Url,
    bearer_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteHandle {
    pub execution_id: String,
    pub worker_base_url: Url,
    pub worker_bearer_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct RemoteLaunchRequest<'a> {
    run_id: Uuid,
    lease_epoch: u64,
    task_id: &'a str,
    language: &'a WorkerLanguage,
    browser_engine: &'a BrowserEngine,
    execution_mode: &'a ExecutionMode,
    source_revision: Option<&'a str>,
    timeout_secs: u64,
    event_callback_url: String,
    event_token: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteLaunchResponse {
    execution_id: String,
    worker_base_url: String,
    #[serde(default)]
    worker_bearer_token: Option<String>,
}

impl RemoteLauncher {
    pub fn from_env() -> Result<Self, String> {
        if std::env::var_os("TKDA_SCINTILLA_TOKEN").is_some() {
            return Err(
                "TKDA_SCINTILLA_TOKEN is not supported; use TKDA_SCINTILLA_TOKEN_FILE".to_owned(),
            );
        }

        let launch_url = validate_service_url(
            &std::env::var("TKDA_SCINTILLA_LAUNCH_URL")
                .map_err(|_| "TKDA_SCINTILLA_LAUNCH_URL is required for execution_target=scintilla")?,
            "TKDA_SCINTILLA_LAUNCH_URL",
            false,
        )?;
        let public_base_url = validate_service_url(
            &std::env::var("TKDA_PUBLIC_BASE_URL")
                .map_err(|_| "TKDA_PUBLIC_BASE_URL is required for execution_target=scintilla")?,
            "TKDA_PUBLIC_BASE_URL",
            false,
        )?;

        let bearer_token = match std::env::var("TKDA_SCINTILLA_TOKEN_FILE") {
            Ok(path) => Some(read_secret_file(&path, "TKDA_SCINTILLA_TOKEN_FILE")?),
            Err(std::env::VarError::NotPresent) if is_literal_loopback(&launch_url) => None,
            Err(std::env::VarError::NotPresent) => {
                return Err(
                    "TKDA_SCINTILLA_TOKEN_FILE is required for non-loopback Scintilla launch URLs"
                        .to_owned(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "TKDA_SCINTILLA_TOKEN_FILE is not valid Unicode: {error}"
                ));
            }
        };

        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(REMOTE_HTTP_TIMEOUT_SECONDS))
            .redirect(Policy::none())
            .build()
            .map_err(|error| format!("failed to build remote launcher HTTP client: {error}"))?;

        return Ok(Self {
            http,
            launch_url,
            public_base_url,
            bearer_token,
        });
    }

    pub async fn launch(
        &self,
        run_id: Uuid,
        lease_epoch: u64,
        request: &CreateRunRequest,
        event_token: &str,
    ) -> Result<RemoteHandle, String> {
        validate_secret_value("event token", event_token)?;

        let callback_base = self.public_base_url.as_str().trim_end_matches('/');
        let event_callback_url = format!("{callback_base}/v1/runs/{run_id}/events");
        let payload = RemoteLaunchRequest {
            run_id,
            lease_epoch,
            task_id: &request.task_id,
            language: &request.language,
            browser_engine: &request.browser_engine,
            execution_mode: &request.execution_mode,
            source_revision: request.source_revision.as_deref(),
            timeout_secs: request.timeout_secs,
            event_callback_url,
            event_token,
        };

        let response = self
            .authorize(self.http.post(self.launch_url.clone()))
            .json(&payload)
            .send()
            .await
            .map_err(|error| format!("Scintilla launch request failed: {error}"))?;
        let status = response.status();
        let body = read_bounded_body(response, "Scintilla launch response").await?;
        if !status.is_success() {
            return Err(format!(
                "Scintilla launch returned HTTP {status}: {}",
                bounded_diagnostic(&body, 2_000)
            ));
        }

        let payload: RemoteLaunchResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("invalid Scintilla launch response: {error}"))?;
        if !safe_identifier(&payload.execution_id, 256) {
            return Err("Scintilla launch response execution_id is invalid".to_owned());
        }

        let worker_base_url =
            validate_service_url(&payload.worker_base_url, "worker_base_url", false)?;
        let worker_bearer_token = match payload.worker_bearer_token {
            Some(token) => {
                validate_secret_value("worker bearer token", &token)?;
                Some(token)
            }
            None if is_literal_loopback(&worker_base_url) => None,
            None => {
                return Err(
                    "remote worker response must include a bearer token for non-loopback workers"
                        .to_owned(),
                );
            }
        };

        return Ok(RemoteHandle {
            execution_id: payload.execution_id,
            worker_base_url,
            worker_bearer_token,
        });
    }

    pub async fn send_command(
        &self,
        handle: &RemoteHandle,
        command: &WorkerCommand,
    ) -> Result<(), String> {
        let command_url = format!(
            "{}/v1/command",
            handle.worker_base_url.as_str().trim_end_matches('/')
        );
        let mut request = self.http.post(command_url).json(command);
        if let Some(token) = handle.worker_bearer_token.as_deref() {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("remote worker command failed: {error}"))?;
        let status = response.status();
        let body = read_bounded_body(response, "remote worker response").await?;
        if !status.is_success() {
            return Err(format!(
                "remote worker returned HTTP {status}: {}",
                bounded_diagnostic(&body, 2_000)
            ));
        }

        return Ok(());
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = self.bearer_token.as_deref() {
            return request.bearer_auth(token);
        }

        return request;
    }
}

fn validate_service_url(raw: &str, label: &str, require_root: bool) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| format!("{label} must be a valid URL"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || (require_root && !matches!(url.path(), "" | "/"))
    {
        return Err(format!(
            "{label} must be a credential-free URL without query or fragment{}",
            if require_root { " and must use the root path" } else { "" }
        ));
    }

    if url.scheme() == "https" || (url.scheme() == "http" && is_literal_loopback(&url)) {
        return Ok(url);
    }

    return Err(format!(
        "{label} must use HTTPS unless it targets literal loopback"
    ));
}

fn is_literal_loopback(url: &Url) -> bool {
    return url
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback());
}

fn read_secret_file(raw_path: &str, label: &str) -> Result<String, String> {
    let path = Path::new(raw_path);
    if !path.is_absolute() {
        return Err(format!("{label} must be an absolute path"));
    }
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| format!("failed to inspect {label}: {error}"))?;
    if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
        return Err(format!("{label} must reference a regular non-symlink file"));
    }

    let file = File::open(path).map_err(|error| format!("failed to open {label}: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect opened {label}: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
            return Err(format!("{label} changed between admission and open"));
        }
        let current = fs::symlink_metadata(path)
            .map_err(|error| format!("failed to re-inspect {label}: {error}"))?;
        if current.file_type().is_symlink()
            || current.dev() != metadata.dev()
            || current.ino() != metadata.ino()
        {
            return Err(format!("{label} changed while acquiring the secret snapshot"));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!("{label} must use mode 0600"));
        }
    }

    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SECRET_FILE_BYTES {
        return Err(format!(
            "{label} must contain 1..={MAX_SECRET_FILE_BYTES} bytes"
        ));
    }

    let mut value = String::new();
    file.take(MAX_SECRET_FILE_BYTES + 1)
        .read_to_string(&mut value)
        .map_err(|error| format!("failed to read {label}: {error}"))?;
    if value.len() as u64 > MAX_SECRET_FILE_BYTES {
        return Err(format!("{label} exceeds {MAX_SECRET_FILE_BYTES} bytes"));
    }
    let value = value.trim();
    validate_secret_value(label, value)?;
    return Ok(value.to_owned());
}

fn validate_secret_value(label: &str, value: &str) -> Result<(), String> {
    if value.len() < 32
        || value.len() > 4096
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{label} must contain 32..=4096 non-whitespace characters"
        ));
    }
    Ok(())
}

async fn read_bounded_body(mut response: Response, label: &str) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REMOTE_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "{label} exceeds {MAX_REMOTE_RESPONSE_BYTES} bytes"
        ));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("failed to read {label}: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_REMOTE_RESPONSE_BYTES {
            return Err(format!(
                "{label} exceeds {MAX_REMOTE_RESPONSE_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn bounded_diagnostic(bytes: &[u8], max_chars: usize) -> String {
    let value = String::from_utf8_lossy(bytes);
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn safe_identifier(value: &str, max_chars: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_urls_are_https_or_literal_loopback() {
        assert!(validate_service_url("https://scintilla.example/v1/launch", "URL", false).is_ok());
        assert!(validate_service_url("http://127.0.0.1:8080/v1/launch", "URL", false).is_ok());
        assert!(validate_service_url("http://localhost:8080/v1/launch", "URL", false).is_err());
        assert!(validate_service_url("http://10.0.0.5:8080/v1/launch", "URL", false).is_err());
        assert!(validate_service_url("https://user:pass@scintilla.example/v1/launch", "URL", false).is_err());
        assert!(validate_service_url("https://worker.example/prefix", "URL", false).is_ok());
    }

    #[test]
    fn remote_identifiers_and_secrets_are_bounded() {
        assert!(safe_identifier("execution-1", 256));
        assert!(!safe_identifier("../execution", 256));
        assert!(validate_secret_value("secret", &"a".repeat(32)).is_ok());
        assert!(validate_secret_value("secret", "too short").is_err());
    }

    #[test]
    fn diagnostics_strip_controls_and_bound_length() {
        let value = bounded_diagnostic(b"line1\nline2\0tail", 12);
        assert!(!value.chars().any(char::is_control));
        assert!(value.chars().count() <= 12);
    }
}
