#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export DPM_E2E_ENGINE_KIND=postgres
export DPM_E2E_DATABASE_NAME="${DPM_E2E_DATABASE_NAME:-declmig_target}"
export DPM_E2E_ADMIN_URL="${DPM_E2E_ADMIN_URL:-postgres://postgres:postgres@127.0.0.1:5432/postgres}"
export DPM_E2E_TARGET_URL="${DPM_E2E_TARGET_URL:-postgres://postgres:postgres@127.0.0.1:5432/declmig_target}"
export DPM_E2E_EXPECTED_DIALECT=PostgreSQL
export PGPASSWORD="${PGPASSWORD:-postgres}"

exec bash "$root/scripts/run_engine_smoke.sh" "$@"
