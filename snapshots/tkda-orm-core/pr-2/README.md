# tkda-orm-core

Private Takoda Automation persistence implementation for task/run/schedule execution state.

The first durable slice focuses on browser-run ownership because remote jobs can live for 20-120 minutes and may move between supervisors or Scintilla allocations. `migrations/0001_run_fencing.sql` persists the ownership watermark, checkpoints, and external-effect receipts.

## Fencing invariant

A distributed lease grant supplies a monotonically increasing `fencing_token`. A supervisor may mutate authoritative run state only while all three values match the persisted owner:

- `owner_id`;
- `fencing_token`;
- an unexpired `lease_expires_at`.

A newer token may take ownership. Equal or lower stale owners cannot reclaim it. PostgreSQL triggers independently reject stale/expired execution evidence, while the SeaORM write path uses the same predicates and `SELECT ... FOR UPDATE` around checkpoint/effect writes.

This is deliberately stronger than the in-process `lease_epoch` in `tkda-main-server.rs`: process-local epochs help diagnostics, but only the shared lease authority's fencing token survives restarts and split-brain supervisors.

## Persistence model

- `tkda_runs`: durable status, execution placement, active owner/fencing watermark, heartbeat and checkpoint cursor.
- `tkda_run_checkpoints`: append-only typed worker/checkpoint evidence guarded by the current fence.
- `tkda_side_effect_receipts`: idempotency ledger for externally visible effects such as form submissions, application submissions, uploads, messages, or provider writes.

Side-effect receipt keys are unique per run. Replaying the same key with the same request digest is idempotent; replaying it with different input is an error.

## ORM parity

SeaORM is the primary runtime implementation. `src/diesel_schema.rs` independently mirrors the PostgreSQL schema so CI compiles both Rust ORM views. Shared contracts remain in `tkda-interfaces`; this repository is server-private implementation state.

## Testing

CI starts PostgreSQL 16, applies the actual migration, and proves that:

1. supervisor A can claim and checkpoint with fencing token 10;
2. supervisor B can claim with newer token 11;
3. supervisor A's later checkpoint and renewal are rejected;
4. supervisor B can record a side effect once;
5. an identical replay is idempotent while a conflicting replay is rejected.
