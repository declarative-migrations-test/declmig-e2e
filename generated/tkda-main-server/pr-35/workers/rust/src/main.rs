use std::{
    io::{self, BufRead, Read, Write},
    time::Duration,
};

use reqwest::{Method, Url, blocking::Client, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};

const ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";
const MAX_WEBDRIVER_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Command {
    #[serde(rename = "type")]
    command_type: String,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    lease_epoch: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    browser_engine: Option<String>,
    #[serde(default)]
    body: Option<Value>,
}

struct WorkerState {
    client: Client,
    upstream_url: String,
    browser_name: String,
    browser_engine: String,
    session_id: Option<String>,
    lease_epoch: Option<u64>,
}

impl WorkerState {
    fn new() -> Result<Self, String> {
        let configured = std::env::var("TKDA_SELENIUM_UPSTREAM_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9515".to_string());
        let upstream_url = validate_selenium_upstream_url(&configured)?;
        let browser_name =
            std::env::var("TKDA_SELENIUM_BROWSER").unwrap_or_else(|_| "chrome".to_string());
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .map_err(|error| format!("failed to build WebDriver HTTP client: {error}"))?;

        Ok(Self {
            client,
            upstream_url,
            browser_name,
            browser_engine: "selenium".to_string(),
            session_id: None,
            lease_epoch: None,
        })
    }

    fn webdriver(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|error| format!("invalid upstream HTTP method: {error}"))?;
        let mut request = self
            .client
            .request(method, format!("{}{}", self.upstream_url, path))
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }

        let mut response = request.send().map_err(|error| {
            format!(
                "WebDriver upstream unavailable at {}: {error}",
                self.upstream_url
            )
        })?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_WEBDRIVER_RESPONSE_BYTES)
        {
            return Err(format!(
                "WebDriver upstream response exceeded {MAX_WEBDRIVER_RESPONSE_BYTES} bytes"
            ));
        }

        let mut bytes = Vec::new();
        response
            .take(MAX_WEBDRIVER_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read WebDriver upstream response: {error}"))?;
        if bytes.len() as u64 > MAX_WEBDRIVER_RESPONSE_BYTES {
            return Err(format!(
                "WebDriver upstream response exceeded {MAX_WEBDRIVER_RESPONSE_BYTES} bytes"
            ));
        }

        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "WebDriver upstream returned HTTP {}: {}",
                status,
                truncate(&detail, 20_000)
            ));
        }

        let payload = serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("invalid WebDriver upstream JSON: {error}"))?;
        let value = payload.get("value").cloned().unwrap_or(Value::Null);
        if let Some(error_code) = value.get("error").and_then(Value::as_str) {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(format!("WebDriver upstream error {error_code}: {message}"));
        }

        Ok(value)
    }

    fn ensure_session(&mut self) -> Result<String, String> {
        if let Some(session_id) = &self.session_id {
            return Ok(session_id.clone());
        }

        let value = self.webdriver(
            "POST",
            "/session",
            Some(&json!({
                "capabilities": {
                    "alwaysMatch": {"browserName": self.browser_name},
                    "firstMatch": [{}]
                }
            })),
        )?;
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "WebDriver upstream session response omitted sessionId".to_string())?
            .to_string();
        self.session_id = Some(session_id.clone());

        Ok(session_id)
    }

    fn find_element(&mut self, selector: &str) -> Result<String, String> {
        let session_id = self.ensure_session()?;
        let value = self.webdriver(
            "POST",
            &format!("/session/{session_id}/element"),
            Some(&json!({"using": "css selector", "value": selector})),
        )?;
        let element_id = value
            .get(ELEMENT_KEY)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "WebDriver element response omitted the W3C element id".to_string())?
            .to_string();

        Ok(element_id)
    }

    fn execute(&mut self, action: &Value) -> Result<Value, String> {
        if self.browser_engine != "selenium" {
            return Err(format!(
                "unsupported: Rust worker currently supports browser_engine=selenium, not {}",
                self.browser_engine
            ));
        }

        let operation = action.get("op").and_then(Value::as_str).unwrap_or_default();
        let session_id = self.ensure_session()?;

        match operation {
            "navigate" => {
                let url = action
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "unsupported: navigate requires url".to_string())?;
                self.webdriver(
                    "POST",
                    &format!("/session/{session_id}/url"),
                    Some(&json!({"url": url})),
                )?;
                let current = self.webdriver("GET", &format!("/session/{session_id}/url"), None)?;
                Ok(json!({"url": current}))
            }
            "url" | "current_url" => {
                let current = self.webdriver("GET", &format!("/session/{session_id}/url"), None)?;
                return Ok(json!({"url": current}));
            }
            "title" => {
                let title = self.webdriver("GET", &format!("/session/{session_id}/title"), None)?;
                Ok(json!({"title": title}))
            }
            "click" | "fill" | "text" => {
                let selector = action
                    .get("selector")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("body");
                let element_id = self.find_element(selector)?;
                let element_path = format!("/session/{session_id}/element/{element_id}");

                if operation == "click" {
                    self.webdriver("POST", &format!("{element_path}/click"), Some(&json!({})))?;
                    return Ok(json!({"ok": true}));
                }

                if operation == "fill" {
                    let value = action
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let characters: Vec<String> = value
                        .chars()
                        .map(|character| character.to_string())
                        .collect();
                    self.webdriver(
                        "POST",
                        &format!("{element_path}/value"),
                        Some(&json!({"text": value, "value": characters})),
                    )?;
                    return Ok(json!({"ok": true}));
                }

                let text = self.webdriver("GET", &format!("{element_path}/text"), None)?;
                Ok(json!({
                    "text": truncate(text.as_str().unwrap_or_default(), 200_000)
                }))
            }
            "screenshot" => {
                let screenshot =
                    self.webdriver("GET", &format!("/session/{session_id}/screenshot"), None)?;
                Ok(json!({
                    "screenshot_base64": screenshot,
                    "omitted": false
                }))
            }
            "sleep" => {
                let milliseconds = action
                    .get("milliseconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(250)
                    .min(30_000);
                std::thread::sleep(Duration::from_millis(milliseconds));
                Ok(json!({"slept_ms": milliseconds}))
            }
            _ => Err(format!(
                "unsupported: unsupported Rust Selenium action {operation:?}"
            )),
        }
    }

    fn close_session(&mut self) {
        let Some(session_id) = self.session_id.take() else {
            return;
        };
        if let Err(error) = self.webdriver("DELETE", &format!("/session/{session_id}"), None) {
            if let Some(lease_epoch) = self.lease_epoch.filter(|epoch| *epoch > 0) {
                emit_event(
                    lease_epoch,
                    json!({
                        "type": "log",
                        "level": "warn",
                        "message": truncate(&error, 20_000)
                    }),
                );
            } else {
                eprintln!("failed to close WebDriver session before start: {error}");
            }
        }
    }
}

fn validate_selenium_upstream_url(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw)
        .map_err(|_| "TKDA_SELENIUM_UPSTREAM_URL must be a valid loopback HTTP URL".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "TKDA_SELENIUM_UPSTREAM_URL must include a host".to_string())?;
    if url.scheme() != "http"
        || !matches!(host, "127.0.0.1" | "::1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "TKDA_SELENIUM_UPSTREAM_URL must be credential-free root HTTP on literal loopback with an explicit port"
                .to_string(),
        );
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn emit(value: Value) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    if serde_json::to_writer(&mut lock, &value).is_ok() {
        let _ = writeln!(lock);
        let _ = lock.flush();
    }
}

fn emit_event(lease_epoch: u64, mut value: Value) {
    if lease_epoch == 0 {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert("lease_epoch".to_string(), Value::from(lease_epoch));
    emit(value);
}

fn active_epoch(state: &WorkerState) -> Option<u64> {
    state.lease_epoch.filter(|epoch| *epoch > 0)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn main() {
    let mut state = match WorkerState::new() {
        Ok(state) => state,
        Err(error) => {
            eprintln!("invalid Selenium worker configuration: {error}");
            return;
        }
    };
    eprintln!(
        "rust adapter boot pid={} upstream={}",
        std::process::id(),
        state.upstream_url
    );

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                if let Some(lease_epoch) = active_epoch(&state) {
                    emit_event(
                        lease_epoch,
                        json!({"type": "failed", "retryable": true, "error": error.to_string()}),
                    );
                } else {
                    eprintln!("worker stdin failed before start: {error}");
                }
                state.close_session();
                return;
            }
        };

        let command: Command = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(error) => {
                if let Some(lease_epoch) = active_epoch(&state) {
                    emit_event(
                        lease_epoch,
                        json!({"type": "failed", "retryable": false, "error": error.to_string()}),
                    );
                } else {
                    eprintln!("invalid worker command before start: {error}");
                }
                continue;
            }
        };

        if command.command_type != "start" && active_epoch(&state).is_none() {
            eprintln!(
                "refusing {} command before a positive lease_epoch is established",
                command.command_type
            );
            continue;
        }

        match command.command_type.as_str() {
            "start" => {
                let Some(lease_epoch) = command.lease_epoch.filter(|epoch| *epoch > 0) else {
                    eprintln!("start command requires positive lease_epoch");
                    state.close_session();
                    return;
                };
                state.lease_epoch = Some(lease_epoch);
                if let Some(browser_engine) = command.browser_engine {
                    state.browser_engine = browser_engine;
                }
                emit_event(lease_epoch, json!({"type": "ready", "transport": "stdio"}));
                emit_event(
                    lease_epoch,
                    json!({
                        "type": "log",
                        "level": "info",
                        "message": format!(
                            "rust worker ready for run {} engine={}",
                            command.run_id.as_deref().unwrap_or("unknown"),
                            state.browser_engine
                        )
                    }),
                );
            }
            "driver" => {
                let Some(lease_epoch) = active_epoch(&state) else {
                    eprintln!("driver command received before start");
                    continue;
                };
                let request_id = command.request_id;
                let action = command.body.unwrap_or(Value::Null);
                match state.execute(&action) {
                    Ok(body) => {
                        emit_event(
                            lease_epoch,
                            json!({
                                "type": "driver_response",
                                "request_id": request_id,
                                "status": 200,
                                "body": body
                            }),
                        );
                    }
                    Err(error) => {
                        let unsupported = error.starts_with("unsupported:");
                        emit_event(
                            lease_epoch,
                            json!({
                                "type": "driver_response",
                                "request_id": request_id,
                                "status": if unsupported { 501 } else { 502 },
                                "body": {
                                    "error": error.trim_start_matches("unsupported: ")
                                }
                            }),
                        );
                    }
                }
            }
            "replan" => {
                let Some(lease_epoch) = active_epoch(&state) else {
                    eprintln!("replan command received before start");
                    continue;
                };
                emit_event(
                    lease_epoch,
                    json!({
                        "type": "needs_replan",
                        "reason": command.reason.unwrap_or_else(|| "replan requested".to_string())
                    }),
                );
            }
            "cancel" => {
                let lease_epoch = active_epoch(&state);
                state.close_session();
                if let Some(lease_epoch) = lease_epoch {
                    emit_event(
                        lease_epoch,
                        json!({
                        "type": "log",
                        "level": "info",
                        "message": format!(
                            "cancelled: {}",
                            command.reason.as_deref().unwrap_or("requested")
                        )
                        }),
                    );
                }
                return;
            }
            unknown => {
                if let Some(lease_epoch) = active_epoch(&state) {
                    emit_event(
                        lease_epoch,
                        json!({
                            "type": "failed",
                            "retryable": false,
                            "error": format!("unknown command type: {unknown}")
                        }),
                    );
                } else {
                    eprintln!("unknown command type before start: {unknown}");
                }
            }
        }
    }

    state.close_session();
}

#[cfg(test)]
mod upstream_policy_tests {
    use super::*;

    #[test]
    fn selenium_upstream_is_literal_loopback_only() {
        assert_eq!(
            validate_selenium_upstream_url("http://127.0.0.1:9515").unwrap(),
            "http://127.0.0.1:9515"
        );
        assert!(validate_selenium_upstream_url("http://[::1]:9515").is_ok());
        assert!(validate_selenium_upstream_url("http://localhost:9515").is_err());
        assert!(validate_selenium_upstream_url("http://10.0.0.1:9515").is_err());
        assert!(validate_selenium_upstream_url("https://127.0.0.1:9515").is_err());
        assert!(validate_selenium_upstream_url("http://127.0.0.1").is_err());
        assert!(validate_selenium_upstream_url("http://user:pass@127.0.0.1:9515").is_err());
        assert!(validate_selenium_upstream_url("http://127.0.0.1:9515/wd/hub").is_err());
        assert!(validate_selenium_upstream_url("http://127.0.0.1:9515?x=1").is_err());
    }
}
