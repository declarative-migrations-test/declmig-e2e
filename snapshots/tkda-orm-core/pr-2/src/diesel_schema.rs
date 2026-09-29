diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Jsonb;
    use diesel::sql_types::Timestamptz;
    use diesel::sql_types::Uuid;

    tkda_runs (run_id) {
        run_id -> Uuid,
        task_id -> Text,
        status -> Text,
        execution_target -> Text,
        execution_id -> Nullable<Text>,
        attempt -> Int4,
        max_retries -> Int4,
        timeout_secs -> Int8,
        owner_id -> Nullable<Text>,
        fencing_token -> Nullable<Int8>,
        lease_expires_at -> Nullable<Timestamptz>,
        last_heartbeat_at -> Nullable<Timestamptz>,
        latest_checkpoint_seq -> Int8,
        last_message -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        terminal_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Jsonb;
    use diesel::sql_types::Timestamptz;
    use diesel::sql_types::Uuid;

    tkda_run_checkpoints (run_id, fencing_token, sequence) {
        run_id -> Uuid,
        sequence -> Int8,
        owner_id -> Text,
        fencing_token -> Int8,
        kind -> Text,
        payload -> Jsonb,
        payload_digest -> Nullable<Text>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Timestamptz;
    use diesel::sql_types::Uuid;

    tkda_side_effect_receipts (run_id, effect_key) {
        run_id -> Uuid,
        effect_key -> Text,
        owner_id -> Text,
        fencing_token -> Int8,
        request_digest -> Text,
        result_digest -> Nullable<Text>,
        provider_reference -> Nullable<Text>,
        created_at -> Timestamptz,
    }
}

diesel::joinable!(tkda_run_checkpoints -> tkda_runs (run_id));
diesel::joinable!(tkda_side_effect_receipts -> tkda_runs (run_id));

diesel::allow_tables_to_appear_in_same_query!(
    tkda_runs,
    tkda_run_checkpoints,
    tkda_side_effect_receipts,
);
