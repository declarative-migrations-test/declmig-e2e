#!/usr/bin/env bash
set -euo pipefail

source_dir="${1:-source}"
source_pin="${2:-pins/source.json}"
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
admin_url="${DPM_E2E_ADMIN_URL:-postgres://postgres:postgres@127.0.0.1:5432/postgres}"
target_url="${DPM_E2E_TARGET_URL:-postgres://postgres:postgres@127.0.0.1:5432/declmig_target}"
engine_identity="${DPM_E2E_ENGINE:-postgres:17}"
workflow_commit="${DPM_E2E_WORKFLOW_COMMIT:-${GITHUB_SHA:-0000000000000000000000000000000000000000}}"
export DPM_BIN="${DPM_BIN:-$source_dir/target/debug/dpm}"

mkdir -p artifacts

source_commit="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["source_commit"])' "$source_pin")"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]
[[ "$workflow_commit" =~ ^[0-9a-f]{40}$ ]]
test -x "$DPM_BIN"

dpm() {
  "$DPM_BIN" "$@"
}

export PGPASSWORD="${PGPASSWORD:-postgres}"

psql "$admin_url" --set=ON_ERROR_STOP=1 --command='DROP DATABASE IF EXISTS declmig_target WITH (FORCE);'
psql "$admin_url" --set=ON_ERROR_STOP=1 --command='CREATE DATABASE declmig_target;'
psql "$target_url" --set=ON_ERROR_STOP=1 --file=fixtures/current.sql

dpm diff \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --out artifacts/plan.json

dpm verify \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app

dpm apply \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --yes

dpm diff \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --fail-on-diff \
  --out artifacts/post-apply.json

psql "$target_url" --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > artifacts/catalog-assertions.txt
SELECT count(*) FROM information_schema.columns
WHERE table_schema = 'app' AND table_name = 'accounts' AND column_name = 'display_name';
SELECT count(*) FROM information_schema.tables
WHERE table_schema = 'app' AND table_name = 'projects';
SELECT count(*) FROM pg_indexes
WHERE schemaname = 'app' AND tablename = 'projects' AND indexname = 'projects_owner_account_id_idx';
SQL

test "$(tr -d '\r' < artifacts/catalog-assertions.txt)" = $'1\n1\n1'

completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
python3 - "$source_commit" "$workflow_commit" "$started_at" "$completed_at" "$DPM_BIN" "$engine_identity" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

source_commit, workflow_commit, started_at, completed_at, dpm_bin, engine_identity = sys.argv[1:]
artifacts = pathlib.Path("artifacts")
binary = pathlib.Path(dpm_bin)
plan = artifacts / "plan.json"
post_apply = artifacts / "post-apply.json"
catalog = artifacts / "catalog-assertions.txt"


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


artifact_digests = {
    "plan.json": digest(plan),
    "post-apply.json": digest(post_apply),
    "catalog-assertions.txt": digest(catalog),
}
for name in (
    "source-lib-tests.log",
    "source-fuzz-tests.log",
    "source-build.log",
    "dpm-version.txt",
):
    path = artifacts / name
    if path.is_file():
        artifact_digests[name] = digest(path)

evidence = {
    "schema_version": 1,
    "source_repository": "declarative-migrations/declarative-postgres-migrate.rs",
    "source_commit": source_commit,
    "workflow_repository": os.environ.get("GITHUB_REPOSITORY", "local/declmig-e2e"),
    "workflow_commit": workflow_commit,
    "run_id": os.environ.get("GITHUB_RUN_ID", "local"),
    "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "local"),
    "scenario": "postgres-diff-verify-apply-empty-post-diff",
    "engine": engine_identity,
    "result": "passed",
    "started_at": started_at,
    "completed_at": completed_at,
    "artifact_sha256": digest(plan),
    "dpm_binary_sha256": digest(binary),
    "artifacts": artifact_digests,
}
(artifacts / "evidence.json").write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
print(json.dumps(evidence, sort_keys=True))
PY
