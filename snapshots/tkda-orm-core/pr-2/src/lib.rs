use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use thiserror::Error;
use uuid::Uuid;

#[cfg(feature = "diesel-parity")]
pub mod diesel_schema;

#[cfg(feature = "sea-orm")]
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

#[derive(Debug, Error)]
pub enum StoreError {
    #[cfg(feature = "sea-orm")]
    #[error("database error: {0}")]
    Database(#[from] DbErr),
    #[error("run ownership is stale or the fenced lease has expired")]
    StaleFence,
    #[error("fencing token must be a positive signed 64-bit integer")]
    InvalidFencingToken,
    #[error("checkpoint sequence is out of order: expected {expected}, supplied {supplied}")]
    CheckpointOutOfOrder { expected: i64, supplied: i64 },
    #[error("side-effect key already exists with a different request digest")]
    IdempotencyConflict,
}

#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    pub run_id: Uuid,
    pub task_id: &'a str,
    pub status: &'a str,
    pub execution_target: &'a str,
    pub max_retries: i32,
    pub timeout_secs: i64,
}

#[derive(Debug, Clone)]
pub struct Ownership<'a> {
    pub run_id: Uuid,
    pub owner_id: &'a str,
    pub fencing_token: i64,
    pub lease_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Checkpoint<'a> {
    pub run_id: Uuid,
    pub sequence: i64,
    pub owner_id: &'a str,
    pub fencing_token: i64,
    pub kind: &'a str,
    pub payload: JsonValue,
    pub payload_digest: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct SideEffectReceipt<'a> {
    pub run_id: Uuid,
    pub effect_key: &'a str,
    pub owner_id: &'a str,
    pub fencing_token: i64,
    pub request_digest: &'a str,
    pub result_digest: Option<&'a str>,
    pub provider_reference: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptWrite {
    Inserted,
    Existing,
}

pub fn accepts_new_fencing_token(current: Option<i64>, proposed: i64) -> bool {
    if proposed <= 0 {
        return false;
    }

    return current.is_none_or(|value| proposed > value);
}

fn validate_token(token: i64) -> Result<(), StoreError> {
    if token <= 0 {
        return Err(StoreError::InvalidFencingToken);
    }

    return Ok(());
}

#[cfg(feature = "sea-orm")]
#[derive(Clone)]
pub struct RunStore {
    db: DatabaseConnection,
}

#[cfg(feature = "sea-orm")]
impl RunStore {
    pub fn new(db: DatabaseConnection) -> Self {
        return Self { db };
    }

    pub async fn insert_run(&self, run: NewRun<'_>) -> Result<bool, StoreError> {
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                INSERT INTO tkda_runs (
                    run_id,
                    task_id,
                    status,
                    execution_target,
                    max_retries,
                    timeout_secs
                )
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (run_id) DO NOTHING
                "#,
                vec![
                    run.run_id.into(),
                    run.task_id.into(),
                    run.status.into(),
                    run.execution_target.into(),
                    run.max_retries.into(),
                    run.timeout_secs.into(),
                ],
            ))
            .await?;

        return Ok(result.rows_affected() == 1);
    }

    /// Claim ownership only with a strictly newer fencing token and a future lease.
    pub async fn claim_run(&self, ownership: Ownership<'_>) -> Result<(), StoreError> {
        validate_token(ownership.fencing_token)?;
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE tkda_runs
                   SET owner_id = $2,
                       fencing_token = $3,
                       lease_expires_at = $4,
                       last_heartbeat_at = clock_timestamp(),
                       updated_at = clock_timestamp()
                 WHERE run_id = $1
                   AND terminal_at IS NULL
                   AND $4 > clock_timestamp()
                   AND (fencing_token IS NULL OR fencing_token < $3)
                RETURNING fencing_token
                "#,
                vec![
                    ownership.run_id.into(),
                    ownership.owner_id.into(),
                    ownership.fencing_token.into(),
                    ownership.lease_expires_at.into(),
                ],
            ))
            .await?;

        if row.is_none() {
            return Err(StoreError::StaleFence);
        }

        return Ok(());
    }

    /// Renew only the exact current owner, before expiry, to a strictly later expiry.
    pub async fn renew_run(&self, ownership: Ownership<'_>) -> Result<(), StoreError> {
        validate_token(ownership.fencing_token)?;
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE tkda_runs
                   SET lease_expires_at = $4,
                       last_heartbeat_at = clock_timestamp(),
                       updated_at = clock_timestamp()
                 WHERE run_id = $1
                   AND owner_id = $2
                   AND fencing_token = $3
                   AND lease_expires_at > clock_timestamp()
                   AND $4 > lease_expires_at
                   AND terminal_at IS NULL
                "#,
                vec![
                    ownership.run_id.into(),
                    ownership.owner_id.into(),
                    ownership.fencing_token.into(),
                    ownership.lease_expires_at.into(),
                ],
            ))
            .await?;

        if result.rows_affected() != 1 {
            return Err(StoreError::StaleFence);
        }

        return Ok(());
    }

    /// Update a non-terminal run once. Once `terminal_at` is set, status is immutable.
    pub async fn update_status(
        &self,
        ownership: Ownership<'_>,
        status: &str,
        last_message: Option<&str>,
        terminal: bool,
    ) -> Result<(), StoreError> {
        validate_token(ownership.fencing_token)?;
        let terminal_at: Option<DateTime<Utc>> = terminal.then(Utc::now);
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE tkda_runs
                   SET status = $4,
                       last_message = $5,
                       terminal_at = COALESCE($6, terminal_at),
                       updated_at = clock_timestamp()
                 WHERE run_id = $1
                   AND owner_id = $2
                   AND fencing_token = $3
                   AND lease_expires_at > clock_timestamp()
                   AND terminal_at IS NULL
                "#,
                vec![
                    ownership.run_id.into(),
                    ownership.owner_id.into(),
                    ownership.fencing_token.into(),
                    status.into(),
                    last_message.into(),
                    terminal_at.into(),
                ],
            ))
            .await?;

        if result.rows_affected() != 1 {
            return Err(StoreError::StaleFence);
        }

        return Ok(());
    }

    /// Append one checkpoint to the current fencing epoch.
    ///
    /// The run row is locked before sequence validation. This serializes
    /// concurrent appends without holding a database lock for the lifetime of
    /// the browser run. Sequence numbers restart at one when a newer fencing
    /// token claims the run.
    pub async fn record_checkpoint(&self, checkpoint: Checkpoint<'_>) -> Result<(), StoreError> {
        validate_token(checkpoint.fencing_token)?;
        let transaction = self.db.begin().await?;
        let latest_sequence = current_checkpoint_sequence(
            &transaction,
            checkpoint.run_id,
            checkpoint.owner_id,
            checkpoint.fencing_token,
        )
        .await?;
        let expected_sequence = latest_sequence.saturating_add(1);

        if checkpoint.sequence != expected_sequence {
            transaction.rollback().await?;
            return Err(StoreError::CheckpointOutOfOrder {
                expected: expected_sequence,
                supplied: checkpoint.sequence,
            });
        }

        transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                INSERT INTO tkda_run_checkpoints (
                    run_id,
                    sequence,
                    owner_id,
                    fencing_token,
                    kind,
                    payload,
                    payload_digest
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                vec![
                    checkpoint.run_id.into(),
                    checkpoint.sequence.into(),
                    checkpoint.owner_id.into(),
                    checkpoint.fencing_token.into(),
                    checkpoint.kind.into(),
                    checkpoint.payload.into(),
                    checkpoint.payload_digest.into(),
                ],
            ))
            .await?;

        let result = transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE tkda_runs
                   SET latest_checkpoint_seq = $4,
                       updated_at = clock_timestamp()
                 WHERE run_id = $1
                   AND owner_id = $2
                   AND fencing_token = $3
                   AND lease_expires_at > clock_timestamp()
                   AND terminal_at IS NULL
                   AND latest_checkpoint_seq = $4 - 1
                "#,
                vec![
                    checkpoint.run_id.into(),
                    checkpoint.owner_id.into(),
                    checkpoint.fencing_token.into(),
                    checkpoint.sequence.into(),
                ],
            ))
            .await?;
        if result.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(StoreError::StaleFence);
        }

        transaction.commit().await?;
        return Ok(());
    }

    pub async fn record_side_effect(
        &self,
        receipt: SideEffectReceipt<'_>,
    ) -> Result<ReceiptWrite, StoreError> {
        validate_token(receipt.fencing_token)?;
        let transaction = self.db.begin().await?;
        assert_current_owner(
            &transaction,
            receipt.run_id,
            receipt.owner_id,
            receipt.fencing_token,
        )
        .await?;

        let result = transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                INSERT INTO tkda_side_effect_receipts (
                    run_id,
                    effect_key,
                    owner_id,
                    fencing_token,
                    request_digest,
                    result_digest,
                    provider_reference
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT (run_id, effect_key) DO NOTHING
                "#,
                vec![
                    receipt.run_id.into(),
                    receipt.effect_key.into(),
                    receipt.owner_id.into(),
                    receipt.fencing_token.into(),
                    receipt.request_digest.into(),
                    receipt.result_digest.into(),
                    receipt.provider_reference.into(),
                ],
            ))
            .await?;

        if result.rows_affected() == 1 {
            transaction.commit().await?;
            return Ok(ReceiptWrite::Inserted);
        }

        let existing = transaction
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                SELECT request_digest
                  FROM tkda_side_effect_receipts
                 WHERE run_id = $1 AND effect_key = $2
                "#,
                vec![receipt.run_id.into(), receipt.effect_key.into()],
            ))
            .await?;
        let Some(existing) = existing else {
            transaction.rollback().await?;
            return Err(StoreError::IdempotencyConflict);
        };
        let existing_digest: String = existing.try_get("", "request_digest")?;
        if existing_digest != receipt.request_digest {
            transaction.rollback().await?;
            return Err(StoreError::IdempotencyConflict);
        }

        transaction.commit().await?;
        return Ok(ReceiptWrite::Existing);
    }
}

#[cfg(feature = "sea-orm")]
async fn current_checkpoint_sequence<C>(
    connection: &C,
    run_id: Uuid,
    owner_id: &str,
    fencing_token: i64,
) -> Result<i64, StoreError>
where
    C: ConnectionTrait,
{
    let row = connection
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            SELECT latest_checkpoint_seq
              FROM tkda_runs
             WHERE run_id = $1
               AND owner_id = $2
               AND fencing_token = $3
               AND lease_expires_at > clock_timestamp()
               AND terminal_at IS NULL
             FOR UPDATE
            "#,
            vec![run_id.into(), owner_id.into(), fencing_token.into()],
        ))
        .await?;

    let Some(row) = row else {
        return Err(StoreError::StaleFence);
    };

    return Ok(row.try_get("", "latest_checkpoint_seq")?);
}

#[cfg(feature = "sea-orm")]
async fn assert_current_owner<C>(
    connection: &C,
    run_id: Uuid,
    owner_id: &str,
    fencing_token: i64,
) -> Result<(), StoreError>
where
    C: ConnectionTrait,
{
    let row = connection
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            SELECT 1 AS owned
              FROM tkda_runs
             WHERE run_id = $1
               AND owner_id = $2
               AND fencing_token = $3
               AND lease_expires_at > clock_timestamp()
               AND terminal_at IS NULL
             FOR UPDATE
            "#,
            vec![run_id.into(), owner_id.into(), fencing_token.into()],
        ))
        .await?;

    if row.is_none() {
        return Err(StoreError::StaleFence);
    }

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_fencing_tokens_are_accepted() {
        assert!(accepts_new_fencing_token(None, 1));
        assert!(accepts_new_fencing_token(Some(41), 42));
    }

    #[test]
    fn stale_equal_and_non_positive_tokens_are_rejected() {
        assert!(!accepts_new_fencing_token(Some(42), 42));
        assert!(!accepts_new_fencing_token(Some(42), 41));
        assert!(!accepts_new_fencing_token(None, 0));
        assert!(!accepts_new_fencing_token(None, -1));
    }
}
