# PostgreSQL and CockroachDB certification audit

Audit date: 2026-08-14

Follow-up audit: 2026-08-31

## Findings

1. The aggregate release-evidence workflow required PostgreSQL but had no
   CockroachDB job, despite DPM documenting both engines as first-class.
2. The smoke contract stopped after one successful apply. It did not prove
   that an idempotent replay stayed empty or preserved existing data.
3. PostgreSQL evidence asserted three catalog objects, but no independent
   artifact compared the portable schema surface with CockroachDB.
4. The immutable DPM source pin predated the current signed `main` commit and
   its successful PostgreSQL/CockroachDB source CI.
5. The first portable scenario primarily bootstrapped new objects. It did not
   independently certify mutation of existing enums, sequences, generated
   columns, defaults, not-null state, indexes, views, or routines.
6. Evidence digests were checked when present, but the comparator did not
   require a complete artifact set or reject an artifact path escaping its
   downloaded evidence directory.

## Hardening applied

- Require `postgres-smoke`, `cockroach-smoke`, and `dual-engine-parity` after
  the repository contract check.
- Run the same SQL fixtures and the same
  `diff -> verify -> apply -> empty diff -> replay -> empty diff` state machine
  against PostgreSQL 17 and CockroachDB 25.2.4.
- Exercise portable tables, identity columns, constraints, ordinary and partial
  expression indexes, a view, a PL/pgSQL trigger function, a stored procedure,
  and a row-level trigger; execute the migrated procedure and prove its trigger
  side effect on both engines.
- Seed a row before migration and prove that both engines retain it after the
  forward migration and no-op replay.
- Record and compare plan operation sequences plus a portable signature of
  columns, constraints, and the explicit supporting index.
- Verify every downloaded artifact against the SHA-256 map in its engine
  evidence before producing dual-engine evidence.
- Reject non-loopback, cross-endpoint, or misnamed runtime database URLs before
  any aggregate smoke-test database is dropped or created.
- Unit-test the evidence verifier's success path and fail-closed behavior for
  artifact tampering, plan-operation drift, and portable catalog drift.
- Add a second live portable-intersection scenario that evolves already-existing
  objects: insert an enum label, alter a sequence, change defaults and not-null
  state, add and backfill ordinary/generated columns, add and enforce a check,
  replace an index and view, replace functions and a procedure, preserve a
  trigger dependency, execute the new behavior, and prove replay remains empty.
- Require every bootstrap and evolution artifact digest, reject malformed
  digests and evidence-root path escapes, and compare both portable signatures.
- At the original audit, pin DPM to signed commit
  `027c81892cafc3693b550eaa74940a17b1705235`; later candidate pins remain
  governed by `pins/source.json` and the repository contract.

## Deliberate boundaries

This aggregate harness certifies the portable intersection and engine parity.
Engine-specific routines, triggers, online failure semantics, drift repair,
locking, and destructive rollback remain independently exercised by the
specialized repositories in `declarative-migrations-test` and by DPM's source
suite. The aggregate job must not weaken those engine-specific assertions or
pretend PostgreSQL-only behavior is portable.
