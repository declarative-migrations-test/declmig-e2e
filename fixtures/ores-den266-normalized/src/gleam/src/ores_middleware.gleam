import gleam/dict.{type Dict}
import gleam/list
import gleam/string

pub const contract_version = "1.0.0"

@external(erlang, "ores_middleware_gleam_rate_limiter", "allow")
fn local_rate_limit_allow(
  key: String,
  capacity: Int,
  refill_per_second: Float,
) -> Bool

pub type Environment {
  Development
  Test
  Staging
  Production
}

pub type IntegrationMode {
  Disabled
  Http
  Embedded
}

pub type Config {
  Config(
    contract_version: String,
    environment: Environment,
    required_capabilities: List(String),
    timeout_ms: Int,
    max_body_bytes: Int,
    request_id_header: String,
    trace_header: String,
    require_https: Bool,
    trusted_proxy_cidrs: List(String),
    rate_limit_enabled: Bool,
    rate_limit_capacity: Int,
    rate_limit_refill_per_second: Float,
    compression_enabled: Bool,
    security_headers_enabled: Bool,
    idempotency_enabled: Bool,
    idempotency_header: String,
    fault_injection_enabled: Bool,
    fault_latency_ms: Int,
    fault_error_rate: Float,
    fault_drop_rate: Float,
    test_auth_bypass_enabled: Bool,
    test_auth_bypass_header: String,
    shared_auth_mode: IntegrationMode,
    shared_auth_fail_open: Bool,
    opto_sync_mode: IntegrationMode,
    opto_sync_fail_open: Bool,
    ores_otel_enabled: Bool,
    service_name: String,
  )
}

pub type ValidationIssue {
  ValidationIssue(path: String, code: String, message: String)
}

pub type Request {
  Request(
    method: String,
    path: String,
    scheme: String,
    headers: Dict(String, String),
    body_size: Int,
    remote_ip: String,
  )
}

pub type Response {
  Response(status: Int, headers: Dict(String, String), body: String)
}

pub type RequestContext {
  RequestContext(
    request_id: String,
    trace_id: String,
    tenant_id: String,
    user_id: String,
    locale: String,
    started_at_unix_ms: Int,
    deadline_unix_ms: Int,
    baggage: Dict(String, String),
  )
}

pub type AuthDecision {
  AuthDecision(
    user_id: String,
    tenant_id: String,
    baggage: Dict(String, String),
  )
}

pub type Hooks {
  Hooks(
    authenticate: fn(Request, RequestContext) -> Result(AuthDecision, String),
    auth_baggage_enricher: fn(Request, RequestContext, AuthDecision) ->
      Dict(String, String),
    resolve_test_identity: fn(Request, RequestContext) ->
      Result(AuthDecision, String),
    authorize_ip: fn(Request, RequestContext) -> Bool,
    rate_limit: fn(Request, RequestContext, Int, Float) -> Bool,
    idempotency_get: fn(String) -> Result(Response, Nil),
    idempotency_put: fn(String, Response) -> Nil,
    compress: fn(Request, Response) -> Response,
    etag: fn(Request, Response) -> Response,
    telemetry_started: fn(RequestContext, Request) -> Nil,
    telemetry_finished: fn(RequestContext, Request, Response, Int) -> Nil,
    sync_observe: fn(RequestContext, Request, Response, Int) ->
      Result(Nil, String),
    schema_capture: fn(Request, Response) -> Nil,
  )
}

pub type Middleware =
  fn(Request, fn(Request) -> Response) -> Response

pub type AdapterDescriptor {
  AdapterDescriptor(
    contract_version: String,
    language: String,
    runtime: String,
    package_name: String,
    framework_adapters: List(String),
    capabilities: List(String),
    operation_symbols: Dict(String, String),
  )
}

pub fn capabilities() -> List(String) {
  [
    "request-context",
    "panic-recovery",
    "request-id",
    "trace-context",
    "structured-logging",
    "metrics-red",
    "deadline-timeout",
    "payload-limit",
    "rate-limit",
    "auth",
    "sync-observer",
    "json",
    "headers",
    "compression",
    "tls-policy",
    "security-headers",
    "idempotency",
    "ip-policy",
    "cache-etag",
    "content-negotiation",
    "fault-injection",
    "test-auth-bypass",
    "schema-capture",
  ]
}

pub fn default_config(service_name: String) -> Config {
  Config(
    contract_version: contract_version,
    environment: Development,
    required_capabilities: capabilities(),
    timeout_ms: 5000,
    max_body_bytes: 2 * 1024 * 1024,
    request_id_header: "x-request-id",
    trace_header: "traceparent",
    require_https: True,
    trusted_proxy_cidrs: ["127.0.0.1/32", "::1/128"],
    rate_limit_enabled: True,
    rate_limit_capacity: 5,
    rate_limit_refill_per_second: 5.0,
    compression_enabled: True,
    security_headers_enabled: True,
    idempotency_enabled: True,
    idempotency_header: "idempotency-key",
    fault_injection_enabled: False,
    fault_latency_ms: 0,
    fault_error_rate: 0.0,
    fault_drop_rate: 0.0,
    test_auth_bypass_enabled: False,
    test_auth_bypass_header: "x-test-auth-bypass",
    shared_auth_mode: Disabled,
    shared_auth_fail_open: False,
    opto_sync_mode: Disabled,
    opto_sync_fail_open: True,
    ores_otel_enabled: True,
    service_name: service_name,
  )
}

pub fn default_hooks() -> Hooks {
  Hooks(
    authenticate: fn(_, _) { Ok(AuthDecision("", "", dict.new())) },
    auth_baggage_enricher: fn(_, _, _) { dict.new() },
    resolve_test_identity: fn(_, _) {
      Error("test identity resolver is not configured")
    },
    authorize_ip: fn(_, _) { True },
    rate_limit: fn(request, _, capacity, refill_per_second) {
      let key = case string.trim(request.remote_ip) {
        "" -> "__unknown_client_ip__"
        value -> value
      }
      local_rate_limit_allow(key, capacity, refill_per_second)
    },
    idempotency_get: fn(_) { Error(Nil) },
    idempotency_put: fn(_, _) { Nil },
    compress: fn(_, response) { response },
    etag: fn(_, response) { response },
    telemetry_started: fn(_, _) { Nil },
    telemetry_finished: fn(_, _, _, _) { Nil },
    sync_observe: fn(_, _, _, _) { Ok(Nil) },
    schema_capture: fn(_, _) { Nil },
  )
}

pub fn validate_config(config: Config) -> List(ValidationIssue) {
  let issues = []
  let issues = case config.contract_version == contract_version {
    True -> issues
    False -> [
      ValidationIssue(
        "/contractVersion",
        "unsupported_version",
        "expected " <> contract_version,
      ),
      ..issues
    ]
  }
  let issues = case config.timeout_ms > 0 {
    True -> issues
    False -> [
      ValidationIssue("/timeoutMs", "range", "timeout must be positive"),
      ..issues
    ]
  }
  let issues = case config.max_body_bytes > 0 {
    True -> issues
    False -> [
      ValidationIssue("/maxBodyBytes", "range", "body limit must be positive"),
      ..issues
    ]
  }
  let issues = case
    config.rate_limit_enabled
    && {
      config.rate_limit_capacity <= 0
      || config.rate_limit_refill_per_second <=. 0.0
    }
  {
    True -> [
      ValidationIssue(
        "/rateLimit",
        "invalid_rate_limit",
        "enabled token bucket requires positive capacity and refill",
      ),
      ..issues
    ]
    False -> issues
  }
  let issues = case
    config.fault_error_rate <. 0.0
    || config.fault_error_rate >. 1.0
    || config.fault_drop_rate <. 0.0
    || config.fault_drop_rate >. 1.0
  {
    True -> [
      ValidationIssue(
        "/faultInjection",
        "range",
        "fault rates must be within 0..=1",
      ),
      ..issues
    ]
    False -> issues
  }
  let issues = case
    config.environment == Production && config.fault_injection_enabled
  {
    True -> [
      ValidationIssue(
        "/faultInjection/enabled",
        "production_forbidden",
        "fault injection is forbidden in production",
      ),
      ..issues
    ]
    False -> issues
  }
  let issues = case
    config.environment == Production && config.test_auth_bypass_enabled
  {
    True -> [
      ValidationIssue(
        "/testAuthBypass/enabled",
        "production_forbidden",
        "test auth bypass is forbidden in production",
      ),
      ..issues
    ]
    False -> issues
  }
  let issues = case config.shared_auth_fail_open {
    True -> [
      ValidationIssue(
        "/sharedAuth/failOpen",
        "auth_fail_open",
        "shared-auth must fail closed",
      ),
      ..issues
    ]
    False -> issues
  }
  let issues =
    list.fold(config.required_capabilities, issues, fn(issues, capability) {
      case list.contains(capabilities(), capability) {
        True -> issues
        False -> [
          ValidationIssue(
            "/requiredCapabilities",
            "unknown_capability",
            capability,
          ),
          ..issues
        ]
      }
    })
  list.reverse(issues)
}

pub fn create_middleware(
  config: Config,
  hooks: Hooks,
) -> Result(Middleware, List(ValidationIssue)) {
  case validate_config(config) {
    [] -> Ok(fn(request, next) { handle(config, hooks, request, next) })
    issues -> Error(issues)
  }
}

fn handle(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
) -> Response {
  case request.body_size > config.max_body_bytes {
    True ->
      problem(413, "payload_too_large", "request body exceeds configured limit")
    False -> handle_transport(config, hooks, request, next)
  }
}

fn handle_transport(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
) -> Response {
  case config.require_https && request.scheme != "https" {
    True -> problem(426, "https_required", "HTTPS is required")
    False -> {
      let now = system_time_ms()
      let context =
        RequestContext(
          request_id: value_or_new(header(
            request.headers,
            config.request_id_header,
          )),
          trace_id: value_or_new(
            trace_id(header(request.headers, config.trace_header)),
          ),
          tenant_id: "",
          user_id: "",
          locale: header(request.headers, "accept-language"),
          started_at_unix_ms: now,
          deadline_unix_ms: now + config.timeout_ms,
          baggage: dict.new(),
        )
      handle_policy(config, hooks, request, next, context)
    }
  }
}

fn handle_policy(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
  context: RequestContext,
) -> Response {
  case hooks.authorize_ip(request, context) {
    False -> problem(403, "ip_policy_denied", "request source is not permitted")
    True -> {
      case
        config.rate_limit_enabled
        && !hooks.rate_limit(
          request,
          context,
          config.rate_limit_capacity,
          config.rate_limit_refill_per_second,
        )
      {
        True -> rate_limit_problem(config.rate_limit_capacity)
        False -> authenticate(config, hooks, request, next, context)
      }
    }
  }
}

fn authenticate(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
  context: RequestContext,
) -> Response {
  let bypass_requested =
    config.test_auth_bypass_enabled
    && header(request.headers, config.test_auth_bypass_header) == "true"
  let decision = case bypass_requested {
    True ->
      case config.environment == Test || config.environment == Staging {
        True -> hooks.resolve_test_identity(request, context)
        False -> Error("test bypass is forbidden")
      }
    False -> hooks.authenticate(request, context)
  }
  case decision {
    Error(_) -> problem(401, "authentication_failed", "authentication failed")
    Ok(auth) -> {
      let baggage = hooks.auth_baggage_enricher(request, context, auth)
      let context =
        RequestContext(
          ..context,
          user_id: auth.user_id,
          tenant_id: auth.tenant_id,
          baggage: baggage,
        )
      case config.shared_auth_mode != Disabled && context.user_id == "" {
        True ->
          problem(
            401,
            "authentication_required",
            "shared-auth did not establish a user",
          )
        False -> dispatch(config, hooks, request, next, context)
      }
    }
  }
}

fn dispatch(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
  context: RequestContext,
) -> Response {
  case config.fault_injection_enabled && config.fault_latency_ms > 0 {
    True -> sleep(config.fault_latency_ms)
    False -> Nil
  }
  case
    config.fault_injection_enabled && random_float() <. config.fault_drop_rate
  {
    True -> problem(503, "fault_drop", "injected transport drop")
    False ->
      case
        config.fault_injection_enabled
        && random_float() <. config.fault_error_rate
      {
        True -> problem(500, "fault_error", "injected middleware error")
        False -> execute(config, hooks, request, next, context)
      }
  }
}

fn execute(
  config: Config,
  hooks: Hooks,
  request: Request,
  next: fn(Request) -> Response,
  context: RequestContext,
) -> Response {
  let idempotency_key = case config.idempotency_enabled {
    True -> header(request.headers, config.idempotency_header)
    False -> ""
  }
  let cached = hooks.idempotency_get(idempotency_key)
  case idempotency_key != "" && result_is_ok(cached) {
    True ->
      result_value(
        cached,
        problem(500, "idempotency_error", "idempotency lookup failed"),
      )
    False -> {
      hooks.telemetry_started(context, request)
      let started = system_time_ms()
      let response =
        run_with_context(context, fn() {
          run_with_deadline(fn() { next(request) }, config.timeout_ms, context)
        })
      let response = case response {
        Ok(response) -> response
        Error("deadline_exceeded") ->
          problem(504, "deadline_exceeded", "request deadline exceeded")
        Error(_) -> problem(500, "internal_error", "request handler failed")
      }
      let response = hooks.etag(request, response)
      let response = case config.compression_enabled {
        True -> hooks.compress(request, response)
        False -> response
      }
      let response = attach_headers(config, context, response)
      let duration = system_time_ms() - started
      hooks.schema_capture(request, response)
      let response = case
        hooks.sync_observe(context, request, response, duration)
      {
        Ok(_) -> response
        Error(_) ->
          case config.opto_sync_fail_open {
            True -> response
            False ->
              problem(
                503,
                "sync_observer_failed",
                "opto-sync observation failed",
              )
          }
      }
      case
        idempotency_key != "" && response.status >= 200 && response.status < 300
      {
        True -> hooks.idempotency_put(idempotency_key, response)
        False -> Nil
      }
      hooks.telemetry_finished(context, request, response, duration)
      response
    }
  }
}

fn attach_headers(
  config: Config,
  context: RequestContext,
  response: Response,
) -> Response {
  let headers =
    response.headers
    |> dict.insert(config.request_id_header, context.request_id)
  let headers = case dict.get(headers, "traceparent") {
    Ok(value) ->
      case normalize_traceparent(value) {
        Ok(value) -> dict.insert(headers, "traceparent", value)
        Error(_) -> dict.delete(headers, "traceparent")
      }
    Error(_) -> headers
  }
  let headers = case config.security_headers_enabled {
    True ->
      headers
      |> dict.insert("x-content-type-options", "nosniff")
      |> dict.insert("x-frame-options", "DENY")
      |> dict.insert("referrer-policy", "strict-origin-when-cross-origin")
      |> dict.insert(
        "strict-transport-security",
        "max-age=31536000; includeSubDomains",
      )
    False -> headers
  }
  Response(response.status, headers, response.body)
}

fn rate_limit_problem(capacity: Int) -> Response {
  let policy = "\"ip-default\""
  let Response(status, headers, body) =
    problem(429, "rate_limited", "rate limit exceeded")
  let headers =
    headers
    |> dict.insert(
      "ratelimit-policy",
      policy <> ";q=" <> int_to_string(capacity) <> ";w=1",
    )
    |> dict.insert("ratelimit", policy <> ";r=0;t=1")
    |> dict.insert("ratelimit-limit", int_to_string(capacity))
    |> dict.insert("ratelimit-remaining", "0")
    |> dict.insert("ratelimit-reset", "1")
    |> dict.insert("retry-after", "1")
    |> dict.insert("x-ores-rate-limit-policy", "ip-default")
    |> dict.insert("x-ores-rate-limit-layer", "application")
    |> dict.insert("x-ores-rate-limit-decision", "denied")
  Response(status, headers, body)
}

fn problem(status: Int, code: String, detail: String) -> Response {
  Response(
    status,
    dict.from_list([#("content-type", "application/problem+json")]),
    "{\"type\":\"urn:ores:middleware:"
      <> code
      <> "\",\"title\":\""
      <> code
      <> "\",\"status\":"
      <> int_to_string(status)
      <> ",\"detail\":\""
      <> detail
      <> "\"}",
  )
}

fn header(headers: Dict(String, String), name: String) -> String {
  case dict.get(headers, string.lowercase(name)) {
    Ok(value) -> value
    Error(_) -> ""
  }
}

fn trace_id(value: String) -> String {
  case string.split(value, "-") {
    [_, trace, ..] ->
      case valid_hex_identifier(trace, 32, "00000000000000000000000000000000") {
        True -> string.lowercase(trace)
        False -> ""
      }
    _ -> ""
  }
}

fn normalize_traceparent(value: String) -> Result(String, Nil) {
  case string.split(value, "-") {
    [version, trace, span, flags] -> {
      let version = string.lowercase(version)
      let trace = string.lowercase(trace)
      let span = string.lowercase(span)
      let flags = string.lowercase(flags)
      case
        version == "00"
        && valid_hex_identifier(trace, 32, "00000000000000000000000000000000")
        && valid_hex_identifier(span, 16, "0000000000000000")
        && valid_hex(flags, 2)
      {
        True -> Ok(version <> "-" <> trace <> "-" <> span <> "-" <> flags)
        False -> Error(Nil)
      }
    }
    _ -> Error(Nil)
  }
}

fn valid_hex_identifier(
  value: String,
  expected_length: Int,
  zero_value: String,
) -> Bool {
  value != zero_value && valid_hex(value, expected_length)
}

fn valid_hex(value: String, expected_length: Int) -> Bool {
  string.length(value) == expected_length
  && list.all(string.to_graphemes(value), fn(character) {
    string.contains("0123456789abcdefABCDEF", character)
  })
}

fn value_or_new(value: String) -> String {
  case value == "" {
    True -> new_id()
    False -> value
  }
}

fn result_is_ok(value: Result(a, b)) -> Bool {
  case value {
    Ok(_) -> True
    Error(_) -> False
  }
}

fn result_value(value: Result(a, b), fallback: a) -> a {
  case value {
    Ok(item) -> item
    Error(_) -> fallback
  }
}

pub fn run_with_context(
  context: RequestContext,
  operation: fn() -> result,
) -> result {
  let previous = current_context()
  put_context(context)
  let output = operation()
  case previous {
    Ok(value) -> put_context(value)
    Error(_) -> clear_context()
  }
  output
}

pub fn descriptor() -> AdapterDescriptor {
  AdapterDescriptor(
    contract_version: contract_version,
    language: "gleam",
    runtime: "erlang-otp",
    package_name: "ores_middleware",
    framework_adapters: ["gleam-http", "mist", "wisp", "cowboy", "otp"],
    capabilities: capabilities(),
    operation_symbols: dict.from_list([
      #("descriptor", "descriptor"),
      #("defaultConfig", "default_config"),
      #("validateConfig", "validate_config"),
      #("createMiddleware", "create_middleware"),
      #("runWithContext", "run_with_context"),
      #("currentContext", "current_context"),
      #("capabilities", "capabilities"),
    ]),
  )
}

pub fn descriptor_json() -> String {
  "{\"contractVersion\":\"1.0.0\",\"language\":\"gleam\",\"runtime\":\"erlang-otp\",\"packageName\":\"ores_middleware\",\"frameworkAdapters\":[\"gleam-http\",\"mist\",\"wisp\",\"cowboy\",\"otp\"],\"capabilities\":[\"request-context\",\"panic-recovery\",\"request-id\",\"trace-context\",\"structured-logging\",\"metrics-red\",\"deadline-timeout\",\"payload-limit\",\"rate-limit\",\"auth\",\"sync-observer\",\"json\",\"headers\",\"compression\",\"tls-policy\",\"security-headers\",\"idempotency\",\"ip-policy\",\"cache-etag\",\"content-negotiation\",\"fault-injection\",\"test-auth-bypass\",\"schema-capture\"],\"operationSymbols\":{\"descriptor\":\"descriptor\",\"defaultConfig\":\"default_config\",\"validateConfig\":\"validate_config\",\"createMiddleware\":\"create_middleware\",\"runWithContext\":\"run_with_context\",\"currentContext\":\"current_context\",\"capabilities\":\"capabilities\"}}"
}

@external(erlang, "ores_middleware_context_ffi", "get_context")
pub fn current_context() -> Result(RequestContext, Nil)

@external(erlang, "ores_middleware_context_ffi", "put_context")
fn put_context(context: RequestContext) -> Nil

@external(erlang, "ores_middleware_context_ffi", "clear_context")
fn clear_context() -> Nil

@external(erlang, "ores_middleware_context_ffi", "run_with_deadline")
fn run_with_deadline(
  operation: fn() -> result,
  timeout_ms: Int,
  context: RequestContext,
) -> Result(result, String)

@external(erlang, "ores_middleware_context_ffi", "system_time_ms")
fn system_time_ms() -> Int

@external(erlang, "ores_middleware_context_ffi", "new_id")
fn new_id() -> String

@external(erlang, "ores_middleware_context_ffi", "random_float")
fn random_float() -> Float

@external(erlang, "ores_middleware_context_ffi", "sleep")
fn sleep(milliseconds: Int) -> Nil

@external(erlang, "erlang", "integer_to_binary")
fn int_to_string(value: Int) -> String
