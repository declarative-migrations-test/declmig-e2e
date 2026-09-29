#![cfg(feature = "sea-orm")]

use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement, TransactionTrait};
use serde_json::json;
use tkda_orm_core::{NewRun, Ownership, RunStore};
use tokio::time::sleep;
use uuid::Uuid;

#[tokio::test]
async fn database_trigger_serializes_direct_evidence_with_reassignment() {
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

    // Interleaving 1: evidence obtains the row lock first. A newer ownership
    // claim must wait until that evidence transaction commits, so the evidence
    // can never commit after the newer fence becomes authoritative.
    let run_id = Uuid::new_v4();
    insert_run(&store, run_id, "evidence-first").await;
    claim_owner_a(&store, run_id).await;

    let evidence_tx = db.begin().await.expect("begin evidence transaction");
    evidence_tx
        .execute(checkpoint_insert(
            run_id,
            1,
            "supervisor-a",
            10,
            "evidence-first",
        ))
        .await
        .expect("current owner direct checkpoint must be admitted");

    let claim_store = store.clone();
    let claim = tokio::spawn(async move {
        return claim_store
            .claim_run(Ownership {
                run_id,
                owner_id: "supervisor-b",
                fencing_token: 11,
                lease_expires_at: Utc::now() + Duration::minutes(2),
            })
            .await;
    });

    sleep(StdDuration::from_millis(150)).await;
    assert!(
        !claim.is_finished(),
        "new ownership must wait while a direct evidence trigger holds the run row lock"
    );

    evidence_tx
        .commit()
        .await
        .expect("commit evidence before reassignment");
    claim
        .await
        .expect("join ownership claim")
        .expect("new owner claims after evidence commits");

    let stale_after_reassignment = db
        .execute(checkpoint_insert(
            run_id,
            2,
            "supervisor-a",
            10,
            "stale-after-reassignment",
        ))
        .await;
    assert!(
        stale_after_reassignment.is_err(),
        "old owner evidence must fail after a newer fence is authoritative"
    );

    // Interleaving 2: reassignment obtains the row lock first. A direct stale
    // insert must block, then re-read the committed owner/fence and fail.
    let second_run_id = Uuid::new_v4();
    insert_run(&store, second_run_id, "reassignment-first").await;
    claim_owner_a(&store, second_run_id).await;

    let reassignment_tx = db.begin().await.expect("begin reassignment transaction");
    reassignment_tx
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE tkda_runs
               SET owner_id = $2,
                   fencing_token = $3,
                   lease_expires_at = clock_timestamp() + interval '2 minutes'
             WHERE run_id = $1
            "#,
            vec![second_run_id.into(), "supervisor-b".into(), 11_i64.into()],
        ))
        .await
        .expect("stage newer owner while retaining the run row lock");

    let stale_db = Database::connect(&database_url)
        .await
        .expect("connect independent stale writer");
    let stale_writer = tokio::spawn(async move {
        return stale_db
            .execute(checkpoint_insert(
                second_run_id,
                1,
                "supervisor-a",
                10,
                "stale-racing-reassignment",
            ))
            .await;
    });

    sleep(StdDuration::from_millis(150)).await;
    assert!(
        !stale_writer.is_finished(),
        "stale evidence must wait while reassignment holds the run row lock"
    );

    reassignment_tx
        .commit()
        .await
        .expect("commit newer ownership generation");
    let stale_result = stale_writer.await.expect("join stale evidence writer");
    assert!(
        stale_result.is_err(),
        "stale evidence must re-check the committed fencing generation and fail"
    );
}

async fn insert_run(store: &RunStore, run_id: Uuid, task_id: &str) {
    assert!(
        store
            .insert_run(NewRun {
                run_id,
                task_id,
                status: "starting",
                execution_target: "scintilla",
                max_retries: 2,
                timeout_secs: 1800,
            })
            .await
            .expect("insert run")
    );
}

async fn claim_owner_a(store: &RunStore, run_id: Uuid) {
    store
        .claim_run(Ownership {
            run_id,
            owner_id: "supervisor-a",
            fencing_token: 10,
            lease_expires_at: Utc::now() + Duration::minutes(2),
        })
        .await
        .expect("owner A claim");
}

fn checkpoint_insert(
    run_id: Uuid,
    sequence: i64,
    owner_id: &str,
    fencing_token: i64,
    kind: &str,
) -> Statement {
    return Statement::from_sql_and_values(
        DbBackend::Postgres,
        r#"
        INSERT INTO tkda_run_checkpoints (
            run_id, sequence, owner_id, fencing_token, kind, payload, payload_digest
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        vec![
            run_id.into(),
            sequence.into(),
            owner_id.into(),
            fencing_token.into(),
            kind.into(),
            json!({"kind": kind, "sequence": sequence}).into(),
            format!("sha256:test-{kind}-{sequence}").into(),
        ],
    );
}
