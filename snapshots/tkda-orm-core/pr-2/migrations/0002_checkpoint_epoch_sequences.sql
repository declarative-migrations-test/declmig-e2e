BEGIN;

-- Checkpoint sequence numbers are scoped to one fencing epoch. A reassigned
-- worker starts its checkpoint stream at sequence 1 again, so historical
-- checkpoints from older epochs must not collide with the new stream.
ALTER TABLE tkda_run_checkpoints
    DROP CONSTRAINT IF EXISTS tkda_run_checkpoints_pkey;

ALTER TABLE tkda_run_checkpoints
    ADD CONSTRAINT tkda_run_checkpoints_pkey
    PRIMARY KEY (run_id, fencing_token, sequence);

-- latest_checkpoint_seq describes the active fencing epoch only. Every new
-- owner/fence starts a fresh monotonic checkpoint stream. Once terminal_at has
-- been set, the durable run authority is immutable even for raw SQL writers.
CREATE OR REPLACE FUNCTION tkda_guard_run_fencing()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.terminal_at IS NOT NULL THEN
        RAISE EXCEPTION 'terminal run % is immutable', OLD.run_id
            USING ERRCODE = '55000';
    END IF;

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

    IF NEW.fencing_token IS DISTINCT FROM OLD.fencing_token THEN
        NEW.latest_checkpoint_seq := 0;
    END IF;

    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;

-- Execution evidence is append-only and may only be written while the run is
-- still live. Enforce this in Postgres as well as in tkda-orm-core so any
-- future writer that bypasses the Rust helper cannot append checkpoints or
-- side-effect receipts after terminalization.
--
-- The authority row is locked while the trigger validates the fence. Without
-- this row lock, a stale insert could validate token N, race with a concurrent
-- ownership advance to N+1, and still commit after the newer owner took over.
CREATE OR REPLACE FUNCTION tkda_assert_current_run_owner()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_owner text;
    current_token bigint;
    current_expiry timestamptz;
    current_terminal_at timestamptz;
BEGIN
    SELECT owner_id, fencing_token, lease_expires_at, terminal_at
      INTO current_owner, current_token, current_expiry, current_terminal_at
      FROM tkda_runs
     WHERE run_id = NEW.run_id
     FOR UPDATE;

    IF current_terminal_at IS NOT NULL
        OR current_owner IS DISTINCT FROM NEW.owner_id
        OR current_token IS DISTINCT FROM NEW.fencing_token
        OR current_expiry IS NULL
        OR current_expiry <= clock_timestamp() THEN
        RAISE EXCEPTION 'stale, expired, or terminal run owner for run %', NEW.run_id
            USING ERRCODE = '40001';
    END IF;

    RETURN NEW;
END;
$$;

COMMIT;
