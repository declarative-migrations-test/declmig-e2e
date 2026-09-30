#!/usr/bin/env bash
set -euo pipefail

export TKDA_BIND="127.0.0.1:18088"
export TKDA_SELENIUM_UPSTREAM_URL="http://127.0.0.1:19515"
export RUST_LOG="warn"

cargo run --quiet > /tmp/tkda-main-server.log 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 80); do
  if curl -fsS http://127.0.0.1:18088/healthz >/dev/null; then
    break
  fi
  sleep 0.25
done

create_payload='{
  "task_id": "ci-python-worker",
  "prompt": "protocol smoke test",
  "language": "python",
  "browser_engine": "selenium",
  "timeout_secs": 1200,
  "max_retries": 0,
  "ai": {"enabled": false, "max_planning_steps": 16, "max_replans": 0}
}'

run_json=$(curl -fsS \
  -H 'content-type: application/json' \
  -d "$create_payload" \
  http://127.0.0.1:18088/v1/runs)
run_id=$(jq -er '.run_id' <<<"$run_json")

for _ in $(seq 1 40); do
  status_json=$(curl -fsS "http://127.0.0.1:18088/v1/runs/$run_id")
  status=$(jq -r '.status' <<<"$status_json")
  if [[ "$status" == "running" ]]; then
    break
  fi
  if [[ "$status" == "failed" || "$status" == "cancelled" || "$status" == "timed_out" ]]; then
    echo "$status_json" >&2
    exit 1
  fi
  sleep 0.1
done

response=$(curl -fsS \
  -H 'content-type: application/json' \
  -d '{"method":"POST","path":"/webdriver","body":{"op":"text","selector":"body"},"timeout_ms":5000}' \
  "http://127.0.0.1:18088/v1/runs/$run_id/driver/wait")

# No ChromeDriver is started in this test. A 502 from the Python adapter proves
# the Rust parent launched the real adapter and correlated its upstream failure.
jq -e '.status == 502 and (.body.error | contains("WebDriver upstream unavailable"))' <<<"$response" >/dev/null

curl -fsS -X POST "http://127.0.0.1:18088/v1/runs/$run_id/cancel" >/dev/null
