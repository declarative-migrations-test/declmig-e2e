#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export DPM_E2E_ENGINE_KIND=cockroach
export DPM_E2E_DATABASE_NAME="${DPM_E2E_DATABASE_NAME:-declmig_target}"
export DPM_E2E_ADMIN_URL="${DPM_E2E_ADMIN_URL:-postgresql://root@127.0.0.1:26257/defaultdb?sslmode=disable}"
export DPM_E2E_TARGET_URL="${DPM_E2E_TARGET_URL:-postgresql://root@127.0.0.1:26257/declmig_target?sslmode=disable}"
export DPM_E2E_EXPECTED_DIALECT=CockroachDB

exec bash "$root/scripts/run_engine_smoke.sh" "$@"
