#![cfg(feature = "sea-orm")]

use chrono::{Duration, Utc};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
use serde_json::json;
use tkda_orm_core::{
    Checkpoint, NewRun, Ownership, ReceiptWrite, RunStore, SideEffectReceipt, StoreError,
};
use uuid::Uuid;

#[tokio::test]
async fn newer_owner_fences_stale_supervisor_and_receipts_are_idempotent() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL is required for the Postgres fencing integration test");
    let db = Database::connect(&database_url)
        .await
        .expect("connect to test Postgres");
    db.execute_unprepared(include_str!("../migrations/0001_run_fencing.sql"))
        .await
        .expect("apply Takoda fencing migration");
    db.execute_unprepared(include_str!(
        "../migrations/0002_checkpoint_epoch_sequences.sql"
    ))
    .await
    .expect("apply checkpoint epoch migration");

    let store = RunStore::new(db.clone());
    let run_id = Uuid::new_v4();
    assert!(
        store
            .insert_run(NewRun {
                run_id,
                task_id: "fencing-integration",
                status: "starting",
                execution_target: "scintilla",
                max_retries: 2,
                timeout_secs: 1800,
            })
            .await
            .expect("insert run")
    );

    let owner_a = Ownership {
        run_id,
        owner_id: "supervisor-a",
        fencing_token: 10,
        lease_expires_at: Utc::now() + Duration::minutes(1),
    };
    store
        .claim_run(owner_a.clone())
        .await
        .expect("owner A claim");
    store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 1,
            owner_id: owner_a.owner_id,
            fencing_token: owner_a.fencing_token,
            kind: "worker_ready",
            payload: json!({"ready": true}),
            payload_digest: Some("sha256:ready-a"),
        })
        .await
        .expect("owner A checkpoint");

    let owner_b = Ownership {
        run_id,
        owner_id: "supervisor-b",
        fencing_token: 11,
        lease_expires_at: Utc::now() + Duration::minutes(1),
    };
    store
        .claim_run(owner_b.clone())
        .await
        .expect("newer owner B claim");

    let heartbeat = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT last_heartbeat_at IS NOT NULL AS present FROM tkda_runs WHERE run_id = $1",
            vec![run_id.into()],
        ))
        .await
        .expect("query heartbeat")
        .expect("run row");
    let heartbeat_present: bool = heartbeat.try_get("", "present").expect("heartbeat flag");
    assert!(heartbeat_present, "claims must persist heartbeat evidence");

    let non_extending_renewal = store.renew_run(owner_b.clone()).await;
    assert!(matches!(non_extending_renewal, Err(StoreError::StaleFence)));

    store
        .renew_run(Ownership {
            run_id,
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            lease_expires_at: owner_b.lease_expires_at + Duration::minutes(1),
        })
        .await
        .expect("current owner can extend lease monotonically");

    store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 1,
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            kind: "worker_ready",
            payload: json!({"ready": true, "epoch": 11}),
            payload_digest: Some("sha256:ready-b"),
        })
        .await
        .expect("new epoch restarts checkpoint sequence at one");

    let duplicate = store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 1,
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            kind: "duplicate",
            payload: json!({"duplicate": true}),
            payload_digest: Some("sha256:duplicate"),
        })
        .await;
    assert!(matches!(
        duplicate,
        Err(StoreError::CheckpointOutOfOrder {
            expected: 2,
            supplied: 1
        })
    ));

    let gap = store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 3,
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            kind: "gap",
            payload: json!({"gap": true}),
            payload_digest: Some("sha256:gap"),
        })
        .await;
    assert!(matches!(
        gap,
        Err(StoreError::CheckpointOutOfOrder {
            expected: 2,
            supplied: 3
        })
    ));

    store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 2,
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            kind: "progress",
            payload: json!({"step": 2}),
            payload_digest: Some("sha256:progress-b"),
        })
        .await
        .expect("strict next checkpoint succeeds");

    let stale_checkpoint = store
        .record_checkpoint(Checkpoint {
            run_id,
            sequence: 2,
            owner_id: owner_a.owner_id,
            fencing_token: owner_a.fencing_token,
            kind: "stale",
            payload: json!({"must": "not persist"}),
            payload_digest: Some("sha256:stale"),
        })
        .await;
    assert!(matches!(stale_checkpoint, Err(StoreError::StaleFence)));

    let stale_renewal = store
        .renew_run(Ownership {
            run_id,
            owner_id: owner_a.owner_id,
            fencing_token: owner_a.fencing_token,
            lease_expires_at: Utc::now() + Duration::minutes(3),
        })
        .await;
    assert!(matches!(stale_renewal, Err(StoreError::StaleFence)));

    let first = store
        .record_side_effect(SideEffectReceipt {
            run_id,
            effect_key: "browser-submit:checkout",
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            request_digest: "sha256:request-1",
            result_digest: Some("sha256:result-1"),
            provider_reference: Some("provider-ref-1"),
        })
        .await
        .expect("owner B receipt");
    assert_eq!(first, ReceiptWrite::Inserted);

    let idempotent_replay = store
        .record_side_effect(SideEffectReceipt {
            run_id,
            effect_key: "browser-submit:checkout",
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            request_digest: "sha256:request-1",
            result_digest: Some("sha256:result-1"),
            provider_reference: Some("provider-ref-1"),
        })
        .await
        .expect("idempotent replay");
    assert_eq!(idempotent_replay, ReceiptWrite::Existing);

    let conflicting_replay = store
        .record_side_effect(SideEffectReceipt {
            run_id,
            effect_key: "browser-submit:checkout",
            owner_id: owner_b.owner_id,
            fencing_token: owner_b.fencing_token,
            request_digest: "sha256:different-request",
            result_digest: Some("sha256:result-2"),
            provider_reference: Some("provider-ref-2"),
        })
        .await;
    assert!(matches!(
        conflicting_replay,
        Err(StoreError::IdempotencyConflict)
    ));

    store
        .update_status(owner_b.clone(), "succeeded", Some("done"), true)
        .await
        .expect("current owner can terminalize run exactly once");
    let terminal_rewrite = store
        .update_status(owner_b.clone(), "failed", Some("late failure"), false)
        .await;
    assert!(matches!(terminal_rewrite, Err(StoreError::StaleFence)));

    let raw_terminal_rewrite = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE tkda_runs
               SET status = 'failed',
                   last_message = 'raw late rewrite'
             WHERE run_id = $1
            "#,
            vec![run_id.into()],
        ))
        .await;
    assert!(
        raw_terminal_rewrite.is_err(),
        "database trigger must reject raw SQL mutation after terminalization"
    );

    let late_checkpoint = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            INSERT INTO tkda_run_checkpoints (
                run_id, sequence, owner_id, fencing_token, kind, payload, payload_digest
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            vec![
                run_id.into(),
                3_i64.into(),
                owner_b.owner_id.into(),
                owner_b.fencing_token.into(),
                "late_checkpoint".into(),
                json!({"must": "not persist"}).into(),
                "sha256:late-checkpoint".into(),
            ],
        ))
        .await;
    assert!(
        late_checkpoint.is_err(),
        "database must reject checkpoint writes after terminalization"
    );

    let late_receipt = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            INSERT INTO tkda_side_effect_receipts (
                run_id, effect_key, owner_id, fencing_token, request_digest
            ) VALUES ($1, $2, $3, $4, $5)
            "#,
            vec![
                run_id.into(),
                "late-effect".into(),
                owner_b.owner_id.into(),
                owner_b.fencing_token.into(),
                "sha256:late-effect".into(),
            ],
        ))
        .await;
    assert!(
        late_receipt.is_err(),
        "database must reject side-effect writes after terminalization"
    );
}
