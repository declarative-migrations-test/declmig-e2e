# tkda-orm-core role

Private persistence implementation for Takoda tasks, schedules, runs, attempts, leases, source revisions, artifacts, checkpoints and audit records. Keep domain contracts in `tkda-interfaces` and domain behavior in `tkda-lib-core`.

Reference `canonical-cloud/canonical-orm-core`, `tkda-interfaces`, `tkda-lib-core`, and the ORES Diesel/SeaORM parity conventions. Persistence names should use snake_case and preserve wire semantics.
