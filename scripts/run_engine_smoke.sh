#!/usr/bin/env bash
set -euo pipefail

source_dir="${1:-source}"
source_pin="${2:-pins/source.json}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
engine_kind="${DPM_E2E_ENGINE_KIND:?DPM_E2E_ENGINE_KIND is required}"
database_name="${DPM_E2E_DATABASE_NAME:-declmig_target}"
admin_url="${DPM_E2E_ADMIN_URL:?DPM_E2E_ADMIN_URL is required}"
target_url="${DPM_E2E_TARGET_URL:?DPM_E2E_TARGET_URL is required}"
expected_dialect="${DPM_E2E_EXPECTED_DIALECT:?DPM_E2E_EXPECTED_DIALECT is required}"
engine_identity="${DPM_E2E_ENGINE:?DPM_E2E_ENGINE is required}"
workflow_commit="${DPM_E2E_WORKFLOW_COMMIT:-${GITHUB_SHA:-0000000000000000000000000000000000000000}}"
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
artifact_dir="${DPM_E2E_ARTIFACT_DIR:-artifacts}"
export DPM_BIN="${DPM_BIN:-$source_dir/target/debug/dpm}"

[[ "$engine_kind" == "postgres" || "$engine_kind" == "cockroach" ]]
[[ "$database_name" =~ ^[a-z][a-z0-9_]*$ ]]
[[ "$workflow_commit" =~ ^[0-9a-f]{40}$ ]]
test -x "$DPM_BIN"
mkdir -p "$artifact_dir"
python3 "$root/scripts/validate_ephemeral_urls.py" "$admin_url" "$target_url" "$database_name"

source_commit="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["source_commit"])' "$source_pin")"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]

dpm() {
  "$DPM_BIN" "$@"
}

case "$engine_kind" in
  postgres)
    psql "$admin_url" --set=ON_ERROR_STOP=1 --command="DROP DATABASE IF EXISTS $database_name WITH (FORCE);"
    ;;
  cockroach)
    psql "$admin_url" --set=ON_ERROR_STOP=1 --command="DROP DATABASE IF EXISTS $database_name CASCADE;"
    ;;
esac
psql "$admin_url" --set=ON_ERROR_STOP=1 --command="CREATE DATABASE $database_name;"
psql "$target_url" --set=ON_ERROR_STOP=1 --file=fixtures/current.sql

psql "$target_url" --set=ON_ERROR_STOP=1 --quiet --tuples-only --no-align \
  --command="INSERT INTO app.accounts(email) VALUES ('preserved@example.test') RETURNING id;" \
  > "$artifact_dir/preserved-account-id.txt"

dpm diff \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --out "$artifact_dir/plan.sql"
grep -Fq "dialect: $expected_dialect" "$artifact_dir/plan.sql"
for object_name in \
  account_audit \
  accounts_audit \
  audit_account_update \
  named_accounts \
  projects_owner_slug_lower_idx \
  set_account_display_name; do
  grep -Fq "$object_name" "$artifact_dir/plan.sql"
done

dpm diff \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --out "$artifact_dir/plan.json"

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

account_id="$(tr -d '\r\n' < "$artifact_dir/preserved-account-id.txt")"
[[ "$account_id" =~ ^[0-9]+$ ]]
psql "$target_url" --set=ON_ERROR_STOP=1 \
  --command="CALL app.set_account_display_name($account_id, 'Preserved Account');" \
  > "$artifact_dir/procedure-execution.txt"

dpm diff \
  --source-sql fixtures/desired.sql \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --fail-on-diff \
  --out "$artifact_dir/post-apply.json"

# A second apply must be a no-op and must not alter the preserved row.
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
  --out "$artifact_dir/post-replay.json"

psql "$target_url" --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/catalog-assertions.txt"
SELECT count(*) FROM information_schema.columns
WHERE table_schema = 'app' AND table_name = 'accounts' AND column_name = 'display_name';
SELECT count(*) FROM information_schema.tables
WHERE table_schema = 'app' AND table_name = 'projects';
SELECT count(*) FROM information_schema.tables
WHERE table_schema = 'app' AND table_name = 'account_audit';
SELECT count(*) FROM pg_indexes
WHERE schemaname = 'app' AND tablename = 'projects' AND indexname = 'projects_owner_account_id_idx';
SELECT count(*) FROM pg_indexes
WHERE schemaname = 'app' AND tablename = 'projects' AND indexname = 'projects_owner_slug_lower_idx';
SELECT count(*) FROM information_schema.views
WHERE table_schema = 'app' AND table_name = 'named_accounts';
SQL
test "$(tr -d '\r' < "$artifact_dir/catalog-assertions.txt")" = $'1\n1\n1\n1\n1\n1'

psql "$target_url" --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/data-assertions.txt"
SELECT count(*) FROM app.accounts WHERE email = 'preserved@example.test';
SELECT email FROM app.accounts WHERE email = 'preserved@example.test';
SELECT display_name FROM app.accounts WHERE email = 'preserved@example.test';
SELECT count(*) FROM app.account_audit WHERE account_id = (SELECT id FROM app.accounts WHERE email = 'preserved@example.test');
SELECT new_display_name FROM app.account_audit WHERE account_id = (SELECT id FROM app.accounts WHERE email = 'preserved@example.test');
SELECT count(*) FROM app.named_accounts WHERE email = 'preserved@example.test';
SQL
test "$(tr -d '\r' < "$artifact_dir/data-assertions.txt")" = $'1\npreserved@example.test\nPreserved Account\n1\nPreserved Account\n1'

psql "$target_url" --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/portable-signature.txt"
SELECT signature
FROM (
    SELECT 'column|' || table_name || '|' || column_name || '|' || data_type || '|' || is_nullable AS signature
    FROM information_schema.columns
    WHERE table_schema = 'app' AND table_name IN ('accounts', 'projects', 'account_audit')
    UNION ALL
    SELECT 'constraint|' || table_name || '|' || constraint_name || '|' || constraint_type
    FROM information_schema.table_constraints
    WHERE table_schema = 'app'
      AND constraint_name IN (
          'accounts_pkey',
          'accounts_email_key',
          'account_audit_pkey',
          'account_audit_account_id_fkey',
          'projects_pkey',
          'projects_owner_slug_key',
          'projects_owner_account_id_fkey'
      )
    UNION ALL
    SELECT 'index|' || tablename || '|' || indexname
    FROM pg_indexes
    WHERE schemaname = 'app'
      AND indexname IN ('projects_owner_account_id_idx', 'projects_owner_slug_lower_idx')
    UNION ALL
    SELECT 'view|' || table_name
    FROM information_schema.views
    WHERE table_schema = 'app' AND table_name = 'named_accounts'
) AS portable
ORDER BY signature;
SQL

completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
python3 - "$source_commit" "$workflow_commit" "$started_at" "$completed_at" "$DPM_BIN" "$engine_identity" "$engine_kind" "$artifact_dir" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

source_commit, workflow_commit, started_at, completed_at, dpm_bin, engine_identity, engine_kind, artifact_dir = sys.argv[1:]
artifacts = pathlib.Path(artifact_dir)
binary = pathlib.Path(dpm_bin)


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


required = (
    "catalog-assertions.txt",
    "data-assertions.txt",
    "plan.json",
    "plan.sql",
    "portable-signature.txt",
    "post-apply.json",
    "post-replay.json",
    "preserved-account-id.txt",
    "procedure-execution.txt",
)
artifact_digests = {name: digest(artifacts / name) for name in required}
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
    "scenario": f"{engine_kind}-diff-verify-apply-replay-empty-post-diff",
    "engine_kind": engine_kind,
    "engine": engine_identity,
    "result": "passed",
    "started_at": started_at,
    "completed_at": completed_at,
    "artifact_sha256": digest(artifacts / "plan.json"),
    "dpm_binary_sha256": digest(binary),
    "artifacts": artifact_digests,
}
(artifacts / "evidence.json").write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
print(json.dumps(evidence, sort_keys=True))
PY
