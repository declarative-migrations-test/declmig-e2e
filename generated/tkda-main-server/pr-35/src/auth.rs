use std::{fs, net::SocketAddr, path::Path};

use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};

const MIN_TOKEN_CHARS: usize = 32;
const MAX_TOKEN_CHARS: usize = 4_096;
const MAX_SECRET_FILE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ApiAuth {
    token: Option<Vec<u8>>,
}

impl ApiAuth {
    pub fn from_env(address: SocketAddr) -> Result<Self, String> {
        let required = parse_bool_env("TKDA_REQUIRE_API_AUTH")?;
        let token_env = std::env::var("TKDA_API_AUTH_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let token_file = std::env::var("TKDA_API_AUTH_TOKEN_FILE")
            .ok()
            .filter(|value| !value.is_empty());

        if token_env.is_some() && token_file.is_some() {
            return Err(
                "configure only one of TKDA_API_AUTH_TOKEN or TKDA_API_AUTH_TOKEN_FILE".to_string(),
            );
        }

        let token = match (token_env, token_file) {
            (Some(value), None) => Some(validate_token(value, "TKDA_API_AUTH_TOKEN")?),
            (None, Some(path)) => Some(read_secret_file(&path)?),
            (None, None) => None,
            (Some(_), Some(_)) => unreachable!("ambiguous token configuration rejected above"),
        };

        if requires_auth(address, required) && token.is_none() {
            return Err(
                "run-control API authentication is required for this bind but no valid token is configured"
                    .to_string(),
            );
        }

        return Ok(Self {
            token: token.map(String::into_bytes),
        });
    }
}

pub async fn enforce_api_auth(
    State(auth): State<ApiAuth>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = auth.token.as_deref() else {
        return next.run(request).await;
    };

    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::as_bytes);

    if supplied.is_some_and(|value| constant_time_eq(value, expected)) {
        return next.run(request).await;
    }

    return (
        StatusCode::UNAUTHORIZED,
        "valid run-control bearer token is required",
    )
        .into_response();
}

fn requires_auth(address: SocketAddr, configured_required: bool) -> bool {
    return configured_required || !address.ip().is_loopback();
}

fn parse_bool_env(name: &str) -> Result<bool, String> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(false);
    };

    return match raw.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{name} must be exactly 'true' or 'false'")),
    };
}

fn read_secret_file(path: &str) -> Result<String, String> {
    let path_ref = Path::new(path);
    if !path_ref.is_absolute() {
        return Err("TKDA_API_AUTH_TOKEN_FILE must be an absolute path".to_string());
    }
    let metadata = fs::symlink_metadata(path_ref)
        .map_err(|error| format!("failed to inspect TKDA_API_AUTH_TOKEN_FILE: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("TKDA_API_AUTH_TOKEN_FILE must be a regular non-symlink file".to_string());
    }
    if metadata.len() == 0 || metadata.len() > MAX_SECRET_FILE_BYTES {
        return Err(format!(
            "TKDA_API_AUTH_TOKEN_FILE must be at most {MAX_SECRET_FILE_BYTES} bytes"
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Dedicated-group read is required by common Kubernetes Secret +
        // fsGroup mounts. Never permit group write/execute or any access for
        // other users.
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(
                "TKDA_API_AUTH_TOKEN_FILE may be group-readable but must not grant group write/execute or any permissions to other users on Unix"
                    .to_string(),
            );
        }
    }

    let value = fs::read_to_string(path_ref)
        .map_err(|error| format!("failed to read TKDA_API_AUTH_TOKEN_FILE: {error}"))?;
    return validate_token(value.trim().to_string(), "TKDA_API_AUTH_TOKEN_FILE");
}

fn validate_token(value: String, source: &str) -> Result<String, String> {
    let chars = value.chars().count();
    if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&chars)
        || value.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "{source} must contain {MIN_TOKEN_CHARS}..={MAX_TOKEN_CHARS} non-whitespace characters"
        ));
    }

    return Ok(value);
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let width = left.len().max(right.len());
    let mut different = left.len() ^ right.len();

    for index in 0..width {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        different |= usize::from(left_byte ^ right_byte);
    }

    return different == 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_bind_always_requires_authentication() {
        let address = "0.0.0.0:8088".parse().expect("socket address");
        assert!(requires_auth(address, false));
    }

    #[test]
    fn loopback_can_remain_unauthenticated_for_local_development() {
        let address = "127.0.0.1:8088".parse().expect("socket address");
        assert!(!requires_auth(address, false));
        assert!(requires_auth(address, true));
    }

    #[test]
    fn token_policy_rejects_short_or_whitespace_secrets() {
        assert!(validate_token("x".repeat(31), "test").is_err());
        assert!(validate_token(format!("{} ", "x".repeat(32)), "test").is_err());
        assert!(validate_token("x".repeat(32), "test").is_ok());
    }

    #[test]
    fn token_comparison_rejects_prefixes_and_mismatches() {
        assert!(constant_time_eq(
            b"abcdefghijklmnopqrstuvwxyz012345",
            b"abcdefghijklmnopqrstuvwxyz012345"
        ));
        assert!(!constant_time_eq(
            b"abcdefghijklmnopqrstuvwxyz012345",
            b"abcdefghijklmnopqrstuvwxyz012346"
        ));
        assert!(!constant_time_eq(
            b"abcdefghijklmnopqrstuvwxyz012345",
            b"abcdefghijklmnopqrstuvwxyz012345extra"
        ));
    }
}
