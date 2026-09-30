# tkda-orm-core architecture

Private persistence adapter for Takoda server processes. Owns Diesel/SeaORM mappings and bounded repository capabilities for tasks, schedules, runs, leases, checkpoints, evidence, synced sources, and usage records. Application servers use explicit repository interfaces rather than sharing database access details.

Reference patterns: `scintilla-run/scintilla-orm-core`, `scintilla-run/scintilla-lib-core`, `declarative-migrations`, and the 20-repo Takoda design inventory in the organization profile.
