import gleam/dict
import gleam/list
import gleeunit
import ores_middleware

pub fn main() {
  gleeunit.main()
}

pub fn descriptor_has_standard_surface_test() {
  let descriptor = ores_middleware.descriptor()
  assert list.length(descriptor.capabilities) == 23
  assert list.length(descriptor.operation_symbols |> dict.to_list) == 7
}

pub fn production_rejects_test_only_middleware_test() {
  let config = ores_middleware.default_config("test")
  let config =
    ores_middleware.Config(
      ..config,
      environment: ores_middleware.Production,
      fault_injection_enabled: True,
      test_auth_bypass_enabled: True,
    )
  assert list.length(ores_middleware.validate_config(config)) >= 2
}

pub fn default_rate_limit_enforces_five_request_burst_per_ip_test() {
  let base = ores_middleware.default_config("rate-limit-test")
  let config =
    ores_middleware.Config(
      ..base,
      require_https: False,
      rate_limit_capacity: 5,
      rate_limit_refill_per_second: 0.000001,
    )
  let assert Ok(middleware) =
    ores_middleware.create_middleware(config, ores_middleware.default_hooks())
  let request =
    ores_middleware.Request(
      "GET",
      "/v1/items",
      "http",
      dict.new(),
      0,
      "198.51.100.250",
    )
  let next = fn(_) { ores_middleware.Response(200, dict.new(), "ok") }

  let ores_middleware.Response(status1, _, _) = middleware(request, next)
  let ores_middleware.Response(status2, _, _) = middleware(request, next)
  let ores_middleware.Response(status3, _, _) = middleware(request, next)
  let ores_middleware.Response(status4, _, _) = middleware(request, next)
  let ores_middleware.Response(status5, _, _) = middleware(request, next)
  let ores_middleware.Response(status6, _, _) = middleware(request, next)

  assert status1 == 200
  assert status2 == 200
  assert status3 == 200
  assert status4 == 200
  assert status5 == 200
  assert status6 == 429
}

pub fn rate_limit_denial_exposes_backpressure_metadata_test() {
  let base = ores_middleware.default_config("rate-limit-test")
  let config =
    ores_middleware.Config(
      ..base,
      require_https: False,
      rate_limit_capacity: 1,
      rate_limit_refill_per_second: 0.000001,
    )
  let assert Ok(middleware) =
    ores_middleware.create_middleware(config, ores_middleware.default_hooks())
  let request =
    ores_middleware.Request(
      "GET",
      "/v1/items",
      "http",
      dict.new(),
      0,
      "198.51.100.251",
    )
  let next = fn(_) { ores_middleware.Response(200, dict.new(), "ok") }

  let ores_middleware.Response(first_status, _, _) = middleware(request, next)
  let ores_middleware.Response(status, headers, _) = middleware(request, next)

  assert first_status == 200
  assert status == 429
  assert dict.get(headers, "retry-after") == Ok("1")
  assert dict.get(headers, "ratelimit-policy") == Ok("\"ip-default\";q=1;w=1")
  assert dict.get(headers, "ratelimit") == Ok("\"ip-default\";r=0;t=1")
  assert dict.get(headers, "ratelimit-limit") == Ok("1")
  assert dict.get(headers, "ratelimit-remaining") == Ok("0")
  assert dict.get(headers, "ratelimit-reset") == Ok("1")
  assert dict.get(headers, "x-ores-rate-limit-policy") == Ok("ip-default")
  assert dict.get(headers, "x-ores-rate-limit-layer") == Ok("application")
  assert dict.get(headers, "x-ores-rate-limit-decision") == Ok("denied")
}
