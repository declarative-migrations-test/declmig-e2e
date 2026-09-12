#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
artifact_dir="${1:?artifact directory is required}"
engine_kind="${DPM_E2E_ENGINE_KIND:?DPM_E2E_ENGINE_KIND is required}"
database_name="${DPM_E2E_DATABASE_NAME:-declmig_target}"
admin_url="${DPM_E2E_ADMIN_URL:?DPM_E2E_ADMIN_URL is required}"
target_url="${DPM_E2E_TARGET_URL:?DPM_E2E_TARGET_URL is required}"
expected_dialect="${DPM_E2E_EXPECTED_DIALECT:?DPM_E2E_EXPECTED_DIALECT is required}"
dpm_bin="${DPM_BIN:?DPM_BIN is required}"

python3 "$root/scripts/validate_ephemeral_urls.py" "$admin_url" "$target_url" "$database_name"
test -x "$dpm_bin"
mkdir -p "$artifact_dir"

dpm() {
  "$dpm_bin" "$@"
}

case "$engine_kind" in
  postgres)
    psql "$admin_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
      --command="DROP DATABASE IF EXISTS $database_name WITH (FORCE);"
    ;;
  cockroach)
    psql "$admin_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
      --command="DROP DATABASE IF EXISTS $database_name CASCADE;"
    ;;
  *)
    echo "unsupported evolution engine: $engine_kind" >&2
    exit 1
    ;;
esac

psql "$admin_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
  --command="CREATE DATABASE $database_name;"
psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
  --file="$root/fixtures/evolution-current.sql"

psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 --quiet --tuples-only --no-align \
  --command="INSERT INTO app.orders(order_code, status, quantity, unit_price_cents, note) VALUES ('order-001', 'pending', 3, 250, 'keep-me') RETURNING id;" \
  > "$artifact_dir/evolution-preserved-order-id.txt"

dpm diff \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --out "$artifact_dir/evolution-plan.sql"
grep -Fq "dialect: $expected_dialect" "$artifact_dir/evolution-plan.sql"
for object_name in \
  calculate_total \
  order_numbers \
  order_status \
  order_summary \
  orders_quantity_check \
  orders_status_idx \
  total_cents \
  update_order_note; do
  grep -Fq "$object_name" "$artifact_dir/evolution-plan.sql"
done

dpm diff \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --out "$artifact_dir/evolution-plan.json"

dpm verify \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app

dpm apply \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --yes

order_id="$(tr -d '\r\n' < "$artifact_dir/evolution-preserved-order-id.txt")"
[[ "$order_id" =~ ^[0-9]+$ ]]
psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
  --command="CALL app.update_order_note($order_id, 'updated-note');" \
  > "$artifact_dir/evolution-procedure-execution.txt"
psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
  --command="INSERT INTO app.orders(order_code, status, unit_price_cents) VALUES ('order-002', 'paid', 500);" \
  > "$artifact_dir/evolution-enum-default-insert.txt"

if psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 \
  --command="INSERT INTO app.orders(order_code, quantity, unit_price_cents) VALUES ('invalid-order', 0, 100);" \
  > "$artifact_dir/evolution-constraint-rejection.txt" 2>&1; then
  echo "orders_quantity_check unexpectedly accepted quantity zero" >&2
  exit 1
fi
grep -Eqi 'check constraint|violates.*constraint' "$artifact_dir/evolution-constraint-rejection.txt"

dpm diff \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --fail-on-diff \
  --out "$artifact_dir/evolution-post-apply.json"

dpm apply \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --yes

dpm diff \
  --source-sql "$root/fixtures/evolution-desired.sql" \
  --target "$target_url" \
  --shadow "$admin_url" \
  --schemas app \
  --format json \
  --fail-on-diff \
  --out "$artifact_dir/evolution-post-replay.json"

psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/evolution-catalog-assertions.txt"
SELECT array_to_string(enum_range(NULL::app.order_status), ',');
SELECT increment FROM information_schema.sequences
WHERE sequence_schema = 'app' AND sequence_name = 'order_numbers';
SELECT count(*) FROM information_schema.columns
WHERE table_schema = 'app' AND table_name = 'orders' AND column_name = 'total_cents' AND is_generated = 'ALWAYS';
SELECT count(*) FROM information_schema.columns
WHERE table_schema = 'app' AND table_name = 'orders' AND column_name = 'note' AND is_nullable = 'NO' AND column_default IS NOT NULL;
SELECT count(*) FROM information_schema.table_constraints
WHERE table_schema = 'app' AND table_name = 'orders' AND constraint_name = 'orders_quantity_check' AND constraint_type = 'CHECK';
SELECT count(*) FROM pg_indexes
WHERE schemaname = 'app' AND tablename = 'orders' AND indexname = 'orders_status_idx'
  AND indexdef ILIKE '%lower(order_code)%' AND indexdef ILIKE '%pending%';
SELECT count(*) FROM information_schema.views
WHERE table_schema = 'app' AND table_name = 'order_summary';
SQL
test "$(tr -d '\r' < "$artifact_dir/evolution-catalog-assertions.txt")" = $'pending,paid,fulfilled\n10\n1\n1\n1\n1\n1'

psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/evolution-data-assertions.txt"
SELECT count(*) FROM app.orders;
SELECT order_code || '|' || status::text || '|' || quantity || '|' || unit_price_cents || '|' || note || '|' || category || '|' || total_cents
FROM app.orders ORDER BY order_code;
SELECT app.calculate_total(quantity, unit_price_cents) FROM app.orders WHERE order_code = 'order-001';
SELECT count(*) FROM app.order_audit WHERE order_id = (SELECT id FROM app.orders WHERE order_code = 'order-001');
SELECT observed_note FROM app.order_audit WHERE order_id = (SELECT id FROM app.orders WHERE order_code = 'order-001');
SELECT count(*) FROM app.order_summary;
SQL
test "$(tr -d '\r' < "$artifact_dir/evolution-data-assertions.txt")" = $'2\norder-001|pending|3|250|updated-note|general|750\norder-002|paid|2|500|new|general|1000\n750\n1\nupdated-note\n2'

psql "$target_url" --no-psqlrc --set=ON_ERROR_STOP=1 --tuples-only --no-align <<'SQL' > "$artifact_dir/evolution-portable-signature.txt"
SELECT signature
FROM (
    SELECT 'column|' || column_name || '|' || data_type || '|' || is_nullable || '|' || is_generated AS signature
    FROM information_schema.columns
    WHERE table_schema = 'app' AND table_name = 'orders'
    UNION ALL
    SELECT 'constraint|' || constraint_name || '|' || constraint_type
    FROM information_schema.table_constraints
    WHERE table_schema = 'app' AND table_name = 'orders'
      AND constraint_name IN ('orders_pkey', 'orders_quantity_check')
    UNION ALL
    SELECT 'index|' || indexname
    FROM pg_indexes
    WHERE schemaname = 'app' AND tablename = 'orders' AND indexname = 'orders_status_idx'
    UNION ALL
    SELECT 'enum|' || array_to_string(enum_range(NULL::app.order_status), ',')
    UNION ALL
    SELECT 'sequence|' || sequence_name || '|' || increment
    FROM information_schema.sequences
    WHERE sequence_schema = 'app' AND sequence_name = 'order_numbers'
    UNION ALL
    SELECT 'view|' || table_name
    FROM information_schema.views
    WHERE table_schema = 'app' AND table_name = 'order_summary'
) AS portable
ORDER BY signature;
SQL
