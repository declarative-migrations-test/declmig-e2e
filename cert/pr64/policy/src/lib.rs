use ores_common_desktop_infra::reload::{
    validate_hot_reload_policy, FrontProxy, HotReloadPolicy, MiddlewareCodeReload,
    RouteReloadBackend, MAX_DRAIN_TIMEOUT_MS as MAX_RELEASE_DRAIN_TIMEOUT_MS,
    MAX_OLD_GENERATIONS as MAX_RELEASE_OLD_GENERATIONS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use thiserror::Error;

pub const HOT_RELOAD_POLICY_FILE: &str = "hot-reload-policy.json";
pub const HOT_RELOAD_POLICY_SCHEMA: &str = "ores.desktop-hot-reload/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationStrategy {
    Auto,
    ProcessGeneration,
    InProcessGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFamily {
    Beam,
    Wasm,
    V8Isolate,
    Graal,
    Lunatic,
    NativeRust,
    NativePony,
    GpuHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeLoadingBoundary {
    Process,
    BeamModule,
    WasmInstance,
    V8Isolate,
    GraalContext,
    LunaticModule,
    GpuExecutionGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCapabilities {
    pub supports_process_generation: bool,
    pub supports_in_process_generation: bool,
    pub supports_parallel_generations: bool,
    pub supports_pre_activation_health_check: bool,
    pub supports_atomic_activation: bool,
    pub supports_inflight_generation_pinning: bool,
    pub supports_graceful_drain: bool,
    pub supports_generation_rollback: bool,
    pub supports_generation_fault_containment: bool,
    pub code_loading_boundary: CodeLoadingBoundary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseActivationPolicy {
    pub runtime_family: RuntimeFamily,
    pub preferred_strategy: ActivationStrategy,
    pub fallback_strategy: ActivationStrategy,
    pub allow_native_dynamic_library_hot_swap: bool,
    pub code_middleware_routes_one_transaction: bool,
    pub drain_timeout_ms: u64,
    pub max_old_generations: u8,
    pub capabilities: GenerationCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationDecision {
    pub requested_strategy: ActivationStrategy,
    pub selected_strategy: ActivationStrategy,
    pub fallback_reason: Option<FallbackReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    InProcessNotSupported,
    ParallelGenerationsNotSupported,
    PreActivationHealthCheckNotSupported,
    AtomicActivationNotSupported,
    InflightGenerationPinningNotSupported,
    GracefulDrainNotSupported,
    GenerationRollbackNotSupported,
    GenerationFaultContainmentNotSupported,
    ManagedCodeLoadingBoundaryRequired,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReleaseActivationError {
    #[error("preferred_strategy must not be auto")]
    AutoPreferredStrategy,
    #[error("fallback_strategy must be process_generation")]
    InvalidFallbackStrategy,
    #[error("process_generation support is required as the fail-closed baseline")]
    MissingProcessGenerationFallback,
    #[error("native dynamic-library/FFI hot swapping is forbidden")]
    UnsafeNativeDynamicLibraryHotSwap,
    #[error("code, middleware, and routes must activate in one generation transaction")]
    SplitActivationTransaction,
    #[error("drain_timeout_ms must be non-zero and no more than 24 hours")]
    InvalidDrainTimeout,
    #[error("max_old_generations must be between 1 and {MAX_RELEASE_OLD_GENERATIONS}")]
    InvalidOldGenerationLimit,
    #[error("runtime family and code-loading boundary are inconsistent")]
    RuntimeBoundaryMismatch,
    #[error("in-process generation requires a managed code-loading boundary")]
    UnmanagedInProcessBoundary,
    #[error("requested activation strategy is unavailable and fallback is disabled: {0:?}")]
    StrategyUnavailable(FallbackReason),
    #[error("process_generation is unavailable")]
    ProcessGenerationUnavailable,
    #[error("release_activation drain_timeout_ms must match default.route_drain_timeout_ms")]
    DrainTimeoutMismatch,
    #[error("release_activation max_old_generations must match default.max_old_generations")]
    OldGenerationLimitMismatch,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HotReloadDocumentError {
    #[error("hot_reload_policy must reference hot-reload-policy.json")]
    InvalidReference,
    #[error("hot-reload policy root/schema is invalid")]
    InvalidDocument,
    #[error("hot-reload policy product does not match appliance product")]
    ProductMismatch,
    #[error("hot-reload policy defaults are invalid: {0}")]
    InvalidDefaults(String),
    #[error("hot-reload policy supported value sets are invalid")]
    InvalidSupportedValues,
    #[error("hot-reload policy invariants are invalid")]
    InvalidInvariants,
    #[error("hot-reload release_activation is invalid: {0}")]
    InvalidReleaseActivation(String),
}

pub fn validate_policy_reference(appliance: &Value) -> Result<(), HotReloadDocumentError> {
    if appliance.get("hot_reload_policy").and_then(Value::as_str) != Some(HOT_RELOAD_POLICY_FILE) {
        return Err(HotReloadDocumentError::InvalidReference);
    }
    return Ok(());
}

pub fn validate_release_activation_policy(
    policy: &ReleaseActivationPolicy,
) -> Result<(), ReleaseActivationError> {
    if policy.preferred_strategy == ActivationStrategy::Auto {
        return Err(ReleaseActivationError::AutoPreferredStrategy);
    }
    if policy.fallback_strategy != ActivationStrategy::ProcessGeneration {
        return Err(ReleaseActivationError::InvalidFallbackStrategy);
    }
    if !policy.capabilities.supports_process_generation {
        return Err(ReleaseActivationError::MissingProcessGenerationFallback);
    }
    if policy.allow_native_dynamic_library_hot_swap {
        return Err(ReleaseActivationError::UnsafeNativeDynamicLibraryHotSwap);
    }
    if !policy.code_middleware_routes_one_transaction {
        return Err(ReleaseActivationError::SplitActivationTransaction);
    }
    if policy.drain_timeout_ms == 0 || policy.drain_timeout_ms > MAX_RELEASE_DRAIN_TIMEOUT_MS {
        return Err(ReleaseActivationError::InvalidDrainTimeout);
    }
    if policy.max_old_generations == 0 || policy.max_old_generations > MAX_RELEASE_OLD_GENERATIONS {
        return Err(ReleaseActivationError::InvalidOldGenerationLimit);
    }
    if !runtime_boundary_matches(
        policy.runtime_family,
        policy.capabilities.code_loading_boundary,
    ) {
        return Err(ReleaseActivationError::RuntimeBoundaryMismatch);
    }
    if policy.capabilities.supports_in_process_generation
        && policy.capabilities.code_loading_boundary == CodeLoadingBoundary::Process
    {
        return Err(ReleaseActivationError::UnmanagedInProcessBoundary);
    }
    return Ok(());
}

pub fn select_activation_strategy(
    policy: &ReleaseActivationPolicy,
    requested_strategy: ActivationStrategy,
    allow_fallback: bool,
) -> Result<ActivationDecision, ReleaseActivationError> {
    validate_release_activation_policy(policy)?;

    let candidate = match requested_strategy {
        ActivationStrategy::Auto => policy.preferred_strategy,
        ActivationStrategy::ProcessGeneration => ActivationStrategy::ProcessGeneration,
        ActivationStrategy::InProcessGeneration => ActivationStrategy::InProcessGeneration,
    };

    if candidate == ActivationStrategy::ProcessGeneration {
        if !policy.capabilities.supports_process_generation {
            return Err(ReleaseActivationError::ProcessGenerationUnavailable);
        }
        return Ok(ActivationDecision {
            requested_strategy,
            selected_strategy: ActivationStrategy::ProcessGeneration,
            fallback_reason: None,
        });
    }

    if let Some(reason) = in_process_blocker(&policy.capabilities) {
        if allow_fallback
            && policy.fallback_strategy == ActivationStrategy::ProcessGeneration
            && policy.capabilities.supports_process_generation
        {
            return Ok(ActivationDecision {
                requested_strategy,
                selected_strategy: ActivationStrategy::ProcessGeneration,
                fallback_reason: Some(reason),
            });
        }
        return Err(ReleaseActivationError::StrategyUnavailable(reason));
    }

    return Ok(ActivationDecision {
        requested_strategy,
        selected_strategy: ActivationStrategy::InProcessGeneration,
        fallback_reason: None,
    });
}

pub fn validate_policy_document(
    policy: &Value,
    expected_product: &str,
) -> Result<(), HotReloadDocumentError> {
    let root = policy
        .as_object()
        .ok_or(HotReloadDocumentError::InvalidDocument)?;

    if root.get("schema").and_then(Value::as_str) != Some(HOT_RELOAD_POLICY_SCHEMA) {
        return Err(HotReloadDocumentError::InvalidDocument);
    }

    if root.get("product").and_then(Value::as_str) != Some(expected_product) {
        return Err(HotReloadDocumentError::ProductMismatch);
    }

    let defaults = root
        .get("default")
        .cloned()
        .ok_or(HotReloadDocumentError::InvalidDocument)?;
    let parsed = serde_json::from_value::<HotReloadPolicy>(defaults)
        .map_err(|error| HotReloadDocumentError::InvalidDefaults(error.to_string()))?;
    validate_hot_reload_policy(&parsed)
        .map_err(|error| HotReloadDocumentError::InvalidDefaults(error.to_string()))?;

    if let Some(value) = root.get("release_activation") {
        let release = serde_json::from_value::<ReleaseActivationPolicy>(value.clone())
            .map_err(|error| HotReloadDocumentError::InvalidReleaseActivation(error.to_string()))?;
        validate_release_activation_policy(&release)
            .map_err(|error| HotReloadDocumentError::InvalidReleaseActivation(error.to_string()))?;
        if release.drain_timeout_ms != parsed.drain_timeout_ms {
            return Err(HotReloadDocumentError::InvalidReleaseActivation(
                ReleaseActivationError::DrainTimeoutMismatch.to_string(),
            ));
        }
        if release.max_old_generations != parsed.max_old_generations {
            return Err(HotReloadDocumentError::InvalidReleaseActivation(
                ReleaseActivationError::OldGenerationLimitMismatch.to_string(),
            ));
        }
    }

    let allowed_front = BTreeSet::from(["none", "nginx", "haproxy", "caddy"]);
    let allowed_routes = BTreeSet::from([
        "beam",
        "native_atomic",
        "external_process",
        "wasm_generation",
        "nginx_worker_generation",
        "haproxy_runtime",
        "caddy_admin",
    ]);
    let allowed_middleware = BTreeSet::from([
        "beam_hot_code",
        "process_generation",
        "wasm_generation",
        "declarative_only",
    ]);

    let front = string_set(root.get("supported_front_proxies"))
        .ok_or(HotReloadDocumentError::InvalidSupportedValues)?;
    let routes = string_set(root.get("supported_route_backends"))
        .ok_or(HotReloadDocumentError::InvalidSupportedValues)?;
    let middleware = string_set(root.get("supported_middleware_code_reload"))
        .ok_or(HotReloadDocumentError::InvalidSupportedValues)?;

    if front.is_empty()
        || routes.is_empty()
        || middleware.is_empty()
        || !front.is_subset(&allowed_front)
        || !routes.is_subset(&allowed_routes)
        || !middleware.is_subset(&allowed_middleware)
        || (routes.contains("nginx_worker_generation") && !front.contains("nginx"))
        || (routes.contains("haproxy_runtime") && !front.contains("haproxy"))
        || (routes.contains("caddy_admin") && !front.contains("caddy"))
    {
        return Err(HotReloadDocumentError::InvalidSupportedValues);
    }

    let default_object = root
        .get("default")
        .and_then(Value::as_object)
        .ok_or(HotReloadDocumentError::InvalidDocument)?;

    let default_front = default_object
        .get("front_proxy")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HotReloadDocumentError::InvalidDefaults("front_proxy is required".to_string())
        })?;
    let default_route = default_object
        .get("route_backend")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HotReloadDocumentError::InvalidDefaults("route_backend is required".to_string())
        })?;
    let default_middleware = default_object
        .get("middleware_code_reload")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HotReloadDocumentError::InvalidDefaults(
                "middleware_code_reload is required".to_string(),
            )
        })?;

    if !front.contains(default_front)
        || !routes.contains(default_route)
        || !middleware.contains(default_middleware)
    {
        return Err(HotReloadDocumentError::InvalidSupportedValues);
    }

    let proxy_support_valid = match parsed.route_backend {
        RouteReloadBackend::NginxWorkerGeneration => front.contains("nginx"),
        RouteReloadBackend::HaproxyRuntime => front.contains("haproxy"),
        RouteReloadBackend::CaddyAdmin => front.contains("caddy"),
        _ => true,
    };
    if !proxy_support_valid {
        return Err(HotReloadDocumentError::InvalidSupportedValues);
    }

    let middleware_name = match parsed.middleware_code_reload {
        MiddlewareCodeReload::BeamHotCode => "beam_hot_code",
        MiddlewareCodeReload::ProcessGeneration => "process_generation",
        MiddlewareCodeReload::WasmGeneration => "wasm_generation",
        MiddlewareCodeReload::DeclarativeOnly => "declarative_only",
    };
    if !middleware.contains(middleware_name) {
        return Err(HotReloadDocumentError::InvalidSupportedValues);
    }

    match parsed.front_proxy {
        FrontProxy::None | FrontProxy::Nginx | FrontProxy::Haproxy | FrontProxy::Caddy => {}
    }

    let invariants = root
        .get("invariants")
        .and_then(Value::as_object)
        .ok_or(HotReloadDocumentError::InvalidInvariants)?;

    let valid_invariants = invariants
        .get("routing_middleware_separate_from_compute")
        .and_then(Value::as_bool)
        == Some(true)
        && invariants
            .get("route_change_restarts_standalone_server")
            .and_then(Value::as_bool)
            == Some(false)
        && invariants
            .get("route_change_restarts_lambda_workers")
            .and_then(Value::as_bool)
            == Some(false)
        && invariants
            .get("arbitrary_native_middleware_in_router_process")
            .and_then(Value::as_bool)
            == Some(false)
        && invariants
            .get("bounded_old_generation_drain")
            .and_then(Value::as_bool)
            == Some(true);

    if !valid_invariants {
        return Err(HotReloadDocumentError::InvalidInvariants);
    }

    return Ok(());
}

pub fn select_release_activation(
    policy: &Value,
    requested_strategy: ActivationStrategy,
    allow_fallback: bool,
) -> Result<ActivationDecision, HotReloadDocumentError> {
    let release = policy.get("release_activation").cloned().ok_or_else(|| {
        HotReloadDocumentError::InvalidReleaseActivation(
            "release_activation is required for strategy selection".to_string(),
        )
    })?;
    let release = serde_json::from_value::<ReleaseActivationPolicy>(release)
        .map_err(|error| HotReloadDocumentError::InvalidReleaseActivation(error.to_string()))?;
    select_activation_strategy(&release, requested_strategy, allow_fallback)
        .map_err(|error| HotReloadDocumentError::InvalidReleaseActivation(error.to_string()))
}

fn runtime_boundary_matches(
    runtime_family: RuntimeFamily,
    code_loading_boundary: CodeLoadingBoundary,
) -> bool {
    return matches!(
        (runtime_family, code_loading_boundary),
        (RuntimeFamily::Beam, CodeLoadingBoundary::BeamModule)
            | (RuntimeFamily::Wasm, CodeLoadingBoundary::WasmInstance)
            | (RuntimeFamily::V8Isolate, CodeLoadingBoundary::V8Isolate)
            | (RuntimeFamily::Graal, CodeLoadingBoundary::GraalContext)
            | (RuntimeFamily::Lunatic, CodeLoadingBoundary::LunaticModule)
            | (RuntimeFamily::NativeRust, CodeLoadingBoundary::Process)
            | (RuntimeFamily::NativePony, CodeLoadingBoundary::Process)
            | (RuntimeFamily::GpuHost, CodeLoadingBoundary::GpuExecutionGeneration)
    );
}

fn in_process_blocker(capabilities: &GenerationCapabilities) -> Option<FallbackReason> {
    if !capabilities.supports_in_process_generation {
        return Some(FallbackReason::InProcessNotSupported);
    }
    if !capabilities.supports_parallel_generations {
        return Some(FallbackReason::ParallelGenerationsNotSupported);
    }
    if !capabilities.supports_pre_activation_health_check {
        return Some(FallbackReason::PreActivationHealthCheckNotSupported);
    }
    if !capabilities.supports_atomic_activation {
        return Some(FallbackReason::AtomicActivationNotSupported);
    }
    if !capabilities.supports_inflight_generation_pinning {
        return Some(FallbackReason::InflightGenerationPinningNotSupported);
    }
    if !capabilities.supports_graceful_drain {
        return Some(FallbackReason::GracefulDrainNotSupported);
    }
    if !capabilities.supports_generation_rollback {
        return Some(FallbackReason::GenerationRollbackNotSupported);
    }
    if !capabilities.supports_generation_fault_containment {
        return Some(FallbackReason::GenerationFaultContainmentNotSupported);
    }
    if capabilities.code_loading_boundary == CodeLoadingBoundary::Process {
        return Some(FallbackReason::ManagedCodeLoadingBoundaryRequired);
    }
    return None;
}

fn string_set(value: Option<&Value>) -> Option<BTreeSet<&str>> {
    let values = value?.as_array()?;
    let mut result = BTreeSet::new();
    for value in values {
        let string = value.as_str()?;
        if string.trim().is_empty() || !result.insert(string) {
            return None;
        }
    }
    return Some(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> Value {
        return json!({
            "schema": HOT_RELOAD_POLICY_SCHEMA,
            "product": "fixture",
            "default": {
                "route_backend": "native_atomic",
                "middleware_code_reload": "process_generation",
                "front_proxy": "none",
                "proxy_managed_application_routes": false,
                "route_drain_timeout_ms": 300000,
                "max_old_generations": 2
            },
            "supported_front_proxies": ["none", "nginx", "haproxy", "caddy"],
            "supported_route_backends": [
                "beam", "native_atomic", "external_process", "wasm_generation",
                "nginx_worker_generation", "haproxy_runtime", "caddy_admin"
            ],
            "supported_middleware_code_reload": [
                "beam_hot_code", "process_generation", "wasm_generation", "declarative_only"
            ],
            "release_activation": {
                "runtime_family": "graal",
                "preferred_strategy": "in_process_generation",
                "fallback_strategy": "process_generation",
                "allow_native_dynamic_library_hot_swap": false,
                "code_middleware_routes_one_transaction": true,
                "drain_timeout_ms": 300000,
                "max_old_generations": 2,
                "capabilities": {
                    "supports_process_generation": true,
                    "supports_in_process_generation": true,
                    "supports_parallel_generations": true,
                    "supports_pre_activation_health_check": true,
                    "supports_atomic_activation": true,
                    "supports_inflight_generation_pinning": true,
                    "supports_graceful_drain": true,
                    "supports_generation_rollback": true,
                    "supports_generation_fault_containment": false,
                    "code_loading_boundary": "graal_context"
                }
            },
            "invariants": {
                "routing_middleware_separate_from_compute": true,
                "route_change_restarts_standalone_server": false,
                "route_change_restarts_lambda_workers": false,
                "arbitrary_native_middleware_in_router_process": false,
                "bounded_old_generation_drain": true
            }
        });
    }

    #[test]
    fn valid_policy_is_accepted() {
        assert_eq!(validate_policy_document(&valid(), "fixture"), Ok(()));
    }

    #[test]
    fn missing_generation_bound_is_rejected() {
        let mut value = valid();
        value["default"]
            .as_object_mut()
            .unwrap()
            .remove("max_old_generations");
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidDefaults(_))
        ));
    }

    #[test]
    fn product_mismatch_is_rejected() {
        assert_eq!(
            validate_policy_document(&valid(), "other"),
            Err(HotReloadDocumentError::ProductMismatch)
        );
    }

    #[test]
    fn supported_proxy_backend_requires_matching_front_proxy() {
        let mut value = valid();
        value["supported_front_proxies"] = json!(["none", "nginx", "haproxy"]);
        assert_eq!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidSupportedValues)
        );
    }

    #[test]
    fn duplicate_supported_values_are_rejected() {
        let mut value = valid();
        value["supported_front_proxies"] = json!(["none", "none"]);
        assert_eq!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidSupportedValues)
        );
    }

    #[test]
    fn missing_release_activation_remains_backward_compatible() {
        let mut value = valid();
        value.as_object_mut().unwrap().remove("release_activation");
        assert_eq!(validate_policy_document(&value, "fixture"), Ok(()));
    }

    #[test]
    fn unsafe_native_dynamic_library_hot_swap_is_rejected() {
        let mut value = valid();
        value["release_activation"]["allow_native_dynamic_library_hot_swap"] = json!(true);
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }

    #[test]
    fn auto_release_activation_falls_back_to_process_generation() {
        let decision = select_release_activation(&valid(), ActivationStrategy::Auto, true)
            .expect("process fallback should be selected");
        assert_eq!(
            decision.selected_strategy,
            ActivationStrategy::ProcessGeneration
        );
        assert_eq!(
            decision.fallback_reason,
            Some(FallbackReason::GenerationFaultContainmentNotSupported)
        );
    }

    #[test]
    fn strict_in_process_request_fails_closed() {
        let mut value = valid();
        value["release_activation"]["capabilities"]["supports_pre_activation_health_check"] =
            json!(false);
        assert!(matches!(
            select_release_activation(&value, ActivationStrategy::InProcessGeneration, false),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }

    #[test]
    fn process_boundary_cannot_claim_in_process_support() {
        let mut value = valid();
        value["release_activation"]["runtime_family"] = json!("native_rust");
        value["release_activation"]["capabilities"]["code_loading_boundary"] = json!("process");
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }

    #[test]
    fn runtime_family_must_match_code_loading_boundary() {
        let mut value = valid();
        value["release_activation"]["capabilities"]["code_loading_boundary"] =
            json!("wasm_instance");
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }

    #[test]
    fn release_activation_drain_policy_cannot_diverge_from_default() {
        let mut value = valid();
        value["release_activation"]["drain_timeout_ms"] = json!(1234);
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));

        let mut value = valid();
        value["release_activation"]["max_old_generations"] = json!(3);
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }

    #[test]
    fn release_activation_rejects_unknown_fields() {
        let mut value = valid();
        value["release_activation"]["capabilities"]["supports_atomc_activation"] = json!(true);
        assert!(matches!(
            validate_policy_document(&value, "fixture"),
            Err(HotReloadDocumentError::InvalidReleaseActivation(_))
        ));
    }
}
