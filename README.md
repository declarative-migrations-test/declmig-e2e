# Declarative Migrations aggregate E2E

This repository certifies an exact `declarative-postgres-migrate.rs` source commit. Its trust role is declared in `config/repository.json`:

- `declarative-migrations-test/declmig-e2e` coordinates candidate, destructive, failure-injection, engine, permission, and cross-repository conformance against ephemeral or explicitly disposable targets.
- `declarative-migrations/declmig-e2e` consumes exact immutable test-org evidence and gates stable release promotion. It never owns destructive targets.

## Required first checks

The `Declarative migrations aggregate E2E` workflow runs two required jobs:

1. `contract` validates the repository identity, mode, full source commit SHA, production-credential prohibition, fixtures, and configuration digests.
2. `postgres-smoke` checks out that exact source revision, runs its library/property tests, builds the `dpm` binary, and performs a PostgreSQL `diff` → `verify` → `apply` → empty post-apply diff with live catalog assertions.

Formatting and strict Clippy remain source-repository quality gates. The aggregate harness intentionally does not reformat an immutable historical tree with a newer moving formatter; source commits may be pinned only after their required source CI passes. This repository owns black-box integration and promotion evidence.

Evidence is written under `artifacts/` and uploaded with exact source/workflow commits, the digest-pinned database engine identity, test/build logs, the `dpm` binary SHA-256, and migration artifact digests. Failure runs upload diagnostics separately and never masquerade as passing evidence. Generated evidence and checked-out source are ignored locally and must not be committed.

## Source updates

Update `pins/source.json` only through a pull request. `source_commit` must be a full lowercase 40-character commit SHA. Never replace it with a branch, tag, abbreviated SHA, or `latest` selector. A pin update must link the source repository’s successful required checks.

## Trust boundaries

- Pull-request workflows receive no environment or cloud secrets.
- Checkout credentials are not persisted.
- GitHub Actions, the Rust toolchain, and the PostgreSQL service image are immutable pins.
- The test repository may target only ephemeral or explicitly disposable databases and must reject production credentials/targets.
- The production repository may consume only exact immutable evidence from the test aggregate.
- Product service conformance requires one `*-lib-core` persistence authority, API-owned product writes, bounded database-enforced web reads, isolated web-state writes, migrator-only DDL, and Shared Auth without product-domain database ownership.

See `config/repository.json`, `pins/source.json`, and `.github/workflows/e2e.yml` for the machine-enforced contract.
