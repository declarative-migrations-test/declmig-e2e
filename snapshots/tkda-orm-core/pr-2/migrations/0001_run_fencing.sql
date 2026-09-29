BEGIN;

CREATE TABLE IF NOT EXISTS tkda_runs (
    run_id uuid PRIMARY KEY,
    task_id text NOT NULL CHECK (length(task_id) BETWEEN 1 AND 128),
    status text NOT NULL CHECK (
        status IN (
            'queued',
            'starting',
            'running',
            'retrying',
            'cancelling',
            'cancelled',
            'succeeded',
            'failed',
            'timed_out'
        )
    ),
    execution_target text NOT NULL CHECK (execution_target IN ('local', 'scintilla')),
    execution_id text,
    attempt integer NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_retries integer NOT NULL DEFAULT 2 CHECK (max_retries BETWEEN 0 AND 10),
    timeout_secs bigint NOT NULL CHECK (timeout_secs BETWEEN 1200 AND 7200),
    owner_id text,
    fencing_token bigint CHECK (fencing_token IS NULL OR fencing_token > 0),
    lease_expires_at timestamptz,
    last_heartbeat_at timestamptz,
    latest_checkpoint_seq bigint NOT NULL DEFAULT 0 CHECK (latest_checkpoint_seq >= 0),
    last_message text CHECK (last_message IS NULL OR length(last_message) <= 20000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    terminal_at timestamptz,
    CHECK (
        (owner_id IS NULL AND fencing_token IS NULL AND lease_expires_at IS NULL)
        OR
        (owner_id IS NOT NULL AND fencing_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS tkda_runs_active_owner_idx
    ON tkda_runs (owner_id, lease_expires_at)
    WHERE terminal_at IS NULL;

CREATE INDEX IF NOT EXISTS tkda_runs_execution_idx
    ON tkda_runs (execution_target, execution_id)
    WHERE execution_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS tkda_run_checkpoints (
    run_id uuid NOT NULL REFERENCES tkda_runs(run_id) ON DELETE CASCADE,
    sequence bigint NOT NULL CHECK (sequence > 0),
    owner_id text NOT NULL,
    fencing_token bigint NOT NULL CHECK (fencing_token > 0),
    kind text NOT NULL CHECK (length(kind) BETWEEN 1 AND 128),
    payload jsonb NOT NULL,
    payload_digest text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, sequence)
);

CREATE INDEX IF NOT EXISTS tkda_run_checkpoints_fence_idx
    ON tkda_run_checkpoints (run_id, fencing_token, sequence DESC);

CREATE TABLE IF NOT EXISTS tkda_side_effect_receipts (
    run_id uuid NOT NULL REFERENCES tkda_runs(run_id) ON DELETE CASCADE,
    effect_key text NOT NULL CHECK (length(effect_key) BETWEEN 1 AND 512),
    owner_id text NOT NULL,
    fencing_token bigint NOT NULL CHECK (fencing_token > 0),
    request_digest text NOT NULL CHECK (length(request_digest) BETWEEN 1 AND 256),
    result_digest text,
    provider_reference text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, effect_key)
);

CREATE INDEX IF NOT EXISTS tkda_side_effect_receipts_fence_idx
    ON tkda_side_effect_receipts (run_id, fencing_token);

CREATE OR REPLACE FUNCTION tkda_guard_run_fencing()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.fencing_token IS NOT NULL THEN
        IF NEW.fencing_token IS NULL OR NEW.fencing_token < OLD.fencing_token THEN
            RAISE EXCEPTION 'stale fencing token for run %: proposed %, current %',
                OLD.run_id, NEW.fencing_token, OLD.fencing_token
                USING ERRCODE = '40001';
        END IF;

        IF NEW.fencing_token = OLD.fencing_token
            AND (OLD.lease_expires_at IS NULL OR OLD.lease_expires_at <= clock_timestamp()) THEN
            RAISE EXCEPTION 'expired fencing token for run %: token %',
                OLD.run_id, OLD.fencing_token
                USING ERRCODE = '40001';
        END IF;
    END IF;

    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tkda_runs_guard_fencing ON tkda_runs;
CREATE TRIGGER tkda_runs_guard_fencing
BEFORE UPDATE ON tkda_runs
FOR EACH ROW
EXECUTE FUNCTION tkda_guard_run_fencing();

CREATE OR REPLACE FUNCTION tkda_assert_current_run_owner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_owner text;
    current_token bigint;
    current_expiry timestamptz;
BEGIN
    SELECT owner_id, fencing_token, lease_expires_at
      INTO current_owner, current_token, current_expiry
      FROM tkda_runs
     WHERE run_id = NEW.run_id;

    IF current_owner IS DISTINCT FROM NEW.owner_id
        OR current_token IS DISTINCT FROM NEW.fencing_token
        OR current_expiry IS NULL
        OR current_expiry <= clock_timestamp() THEN
        RAISE EXCEPTION 'stale or expired run owner for run %', NEW.run_id
            USING ERRCODE = '40001';
    END IF;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tkda_run_checkpoints_current_owner ON tkda_run_checkpoints;
CREATE TRIGGER tkda_run_checkpoints_current_owner
BEFORE INSERT OR UPDATE ON tkda_run_checkpoints
FOR EACH ROW
EXECUTE FUNCTION tkda_assert_current_run_owner();

DROP TRIGGER IF EXISTS tkda_side_effect_receipts_current_owner ON tkda_side_effect_receipts;
CREATE TRIGGER tkda_side_effect_receipts_current_owner
BEFORE INSERT OR UPDATE ON tkda_side_effect_receipts
FOR EACH ROW
EXECUTE FUNCTION tkda_assert_current_run_owner();

CREATE OR REPLACE FUNCTION tkda_forbid_immutable_delete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'Takoda execution evidence is append-only'
        USING ERRCODE = '55000';
END;
$$;

DROP TRIGGER IF EXISTS tkda_run_checkpoints_no_delete ON tkda_run_checkpoints;
CREATE TRIGGER tkda_run_checkpoints_no_delete
BEFORE DELETE ON tkda_run_checkpoints
FOR EACH ROW
EXECUTE FUNCTION tkda_forbid_immutable_delete();

DROP TRIGGER IF EXISTS tkda_side_effect_receipts_no_delete ON tkda_side_effect_receipts;
CREATE TRIGGER tkda_side_effect_receipts_no_delete
BEFORE DELETE ON tkda_side_effect_receipts
FOR EACH ROW
EXECUTE FUNCTION tkda_forbid_immutable_delete();

COMMIT;
