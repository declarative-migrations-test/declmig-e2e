use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_DRAIN_TIMEOUT_MS: u64 = 86_400_000;
pub const MAX_OLD_GENERATIONS: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontProxy {
    None,
    Nginx,
    Haproxy,
    Caddy,
}

/// Backend that owns the live route generation. This plane must remain
/// independent from standalone servers and lambda/actor workers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReloadBackend {
    Beam,
    NativeAtomic,
    ExternalProcess,
    WasmGeneration,
    NginxWorkerGeneration,
    HaproxyRuntime,
    CaddyAdmin,
}

/// How executable middleware code is replaced. Proxy configuration reload is
/// deliberately distinct from arbitrary middleware-code reload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareCodeReload {
    BeamHotCode,
    ProcessGeneration,
    WasmGeneration,
    DeclarativeOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotReloadPolicy {
    pub route_backend: RouteReloadBackend,
    pub middleware_code_reload: MiddlewareCodeReload,
    pub front_proxy: FrontProxy,
    #[serde(rename = "route_drain_timeout_ms")]
    pub drain_timeout_ms: u64,
    pub max_old_generations: u8,
    pub proxy_managed_application_routes: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HotReloadPolicyError {
    #[error("route_drain_timeout_ms must be non-zero")]
    ZeroDrainTimeout,
    #[error("route_drain_timeout_ms must not exceed 24 hours")]
    DrainTimeoutTooLarge,
    #[error("max_old_generations must be between 1 and {MAX_OLD_GENERATIONS}")]
    InvalidOldGenerationLimit,
    #[error("nginx worker-generation routing requires front_proxy=nginx")]
    NginxBackendRequiresNginx,
    #[error("HAProxy runtime routing requires front_proxy=haproxy")]
    HaproxyBackendRequiresHaproxy,
    #[error("Caddy Admin API routing requires front_proxy=caddy")]
    CaddyBackendRequiresCaddy,
    #[error("proxy-managed application routes require nginx, HAProxy, or Caddy route backend")]
    ProxyManagedRoutesRequireProxyBackend,
    #[error("nginx/HAProxy/Caddy route backend requires proxy_managed_application_routes=true")]
    ProxyBackendRequiresManagedRoutes,
}

pub fn validate_hot_reload_policy(policy: &HotReloadPolicy) -> Result<(), HotReloadPolicyError> {
    if policy.drain_timeout_ms == 0 {
        return Err(HotReloadPolicyError::ZeroDrainTimeout);
    }

    if policy.drain_timeout_ms > MAX_DRAIN_TIMEOUT_MS {
        return Err(HotReloadPolicyError::DrainTimeoutTooLarge);
    }

    if policy.max_old_generations == 0 || policy.max_old_generations > MAX_OLD_GENERATIONS {
        return Err(HotReloadPolicyError::InvalidOldGenerationLimit);
    }

    match policy.route_backend {
        RouteReloadBackend::NginxWorkerGeneration
            if policy.front_proxy != FrontProxy::Nginx =>
        {
            return Err(HotReloadPolicyError::NginxBackendRequiresNginx);
        }
        RouteReloadBackend::HaproxyRuntime
            if policy.front_proxy != FrontProxy::Haproxy =>
        {
            return Err(HotReloadPolicyError::HaproxyBackendRequiresHaproxy);
        }
        RouteReloadBackend::CaddyAdmin
            if policy.front_proxy != FrontProxy::Caddy =>
        {
            return Err(HotReloadPolicyError::CaddyBackendRequiresCaddy);
        }
        _ => {}
    }

    let proxy_backend = matches!(
        policy.route_backend,
        RouteReloadBackend::NginxWorkerGeneration
            | RouteReloadBackend::HaproxyRuntime
            | RouteReloadBackend::CaddyAdmin
    );

    if policy.proxy_managed_application_routes && !proxy_backend {
        return Err(HotReloadPolicyError::ProxyManagedRoutesRequireProxyBackend);
    }

    if proxy_backend && !policy.proxy_managed_application_routes {
        return Err(HotReloadPolicyError::ProxyBackendRequiresManagedRoutes);
    }

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_policy() -> HotReloadPolicy {
        return HotReloadPolicy {
            route_backend: RouteReloadBackend::NativeAtomic,
            middleware_code_reload: MiddlewareCodeReload::ProcessGeneration,
            front_proxy: FrontProxy::None,
            drain_timeout_ms: 300_000,
            max_old_generations: 2,
            proxy_managed_application_routes: false,
        };
    }

    #[test]
    fn arbitrary_language_process_generation_is_valid() {
        assert_eq!(validate_hot_reload_policy(&native_policy()), Ok(()));
    }

    #[test]
    fn stable_caddy_edge_can_front_native_router() {
        let mut policy = native_policy();
        policy.front_proxy = FrontProxy::Caddy;

        assert_eq!(validate_hot_reload_policy(&policy), Ok(()));
    }

    #[test]
    fn caddy_owned_routes_require_caddy_front_proxy() {
        let mut policy = native_policy();
        policy.route_backend = RouteReloadBackend::CaddyAdmin;
        policy.middleware_code_reload = MiddlewareCodeReload::DeclarativeOnly;
        policy.proxy_managed_application_routes = true;

        assert_eq!(
            validate_hot_reload_policy(&policy),
            Err(HotReloadPolicyError::CaddyBackendRequiresCaddy)
        );

        policy.front_proxy = FrontProxy::Caddy;
        assert_eq!(validate_hot_reload_policy(&policy), Ok(()));
    }

    #[test]
    fn proxy_owned_routes_cannot_claim_native_backend() {
        let mut policy = native_policy();
        policy.proxy_managed_application_routes = true;

        assert_eq!(
            validate_hot_reload_policy(&policy),
            Err(HotReloadPolicyError::ProxyManagedRoutesRequireProxyBackend)
        );
    }

    #[test]
    fn proxy_backend_requires_proxy_managed_routes() {
        let mut policy = native_policy();
        policy.route_backend = RouteReloadBackend::NginxWorkerGeneration;
        policy.front_proxy = FrontProxy::Nginx;

        assert_eq!(
            validate_hot_reload_policy(&policy),
            Err(HotReloadPolicyError::ProxyBackendRequiresManagedRoutes)
        );

        policy.proxy_managed_application_routes = true;
        assert_eq!(validate_hot_reload_policy(&policy), Ok(()));
    }

    #[test]
    fn historical_generations_are_bounded() {
        let mut policy = native_policy();
        policy.max_old_generations = MAX_OLD_GENERATIONS + 1;

        assert_eq!(
            validate_hot_reload_policy(&policy),
            Err(HotReloadPolicyError::InvalidOldGenerationLimit)
        );
    }

    #[test]
    fn drain_timeout_is_bounded() {
        let mut policy = native_policy();
        policy.drain_timeout_ms = MAX_DRAIN_TIMEOUT_MS + 1;

        assert_eq!(
            validate_hot_reload_policy(&policy),
            Err(HotReloadPolicyError::DrainTimeoutTooLarge)
        );
    }

    #[test]
    fn beam_middleware_does_not_require_beam_route_ownership() {
        let mut policy = native_policy();
        policy.middleware_code_reload = MiddlewareCodeReload::BeamHotCode;

        assert_eq!(validate_hot_reload_policy(&policy), Ok(()));
    }
}
