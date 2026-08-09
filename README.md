# Declarative Migrations aggregate E2E

This repository certifies an exact `declarative-postgres-migrate.rs` source commit. Its trust role is declared in `config/repository.json`:

- `declarative-migrations-test/declmig-e2e` coordinates candidate, destructive, failure-injection, engine, permission, and cross-repository conformance against ephemeral or explicitly disposable targets.
- `declarative-migrations/declmig-e2e` consumes exact immutable test-org evidence and gates stable release promotion. It never owns destructive targets.

## Required first checks

The `Declarative migrations aggregate E2E` workflow runs two required jobs:

1. `contract` validates the repository identity, mode, full source commit SHA, production-credential prohibition, fixtures, and configuration digests.
2. `postgres-smoke` checks out that exact source revision, runs Rust format/strict Clippy/tests, then performs a PostgreSQL `diff` → `verify` → `apply` → empty post-apply diff and live catalog assertions.

Evidence is written under `artifacts/` and uploaded with source/workflow identities and SHA-256 digests. Generated evidence and checked-out source are ignored locally and must not be committed.

## Source updates

Update `pins/source.json` only through a pull request. `source_commit` must be a full lowercase 40-character commit SHA. Never replace it with a branch, tag, abbreviated SHA, or `latest` selector.

## Trust boundaries

- Pull-request workflows receive no environment or cloud secrets.
- Checkout credentials are not persisted.
- GitHub Actions are pinned by full commit SHA.
- The test repository may target only ephemeral or explicitly disposable databases and must reject production credentials/targets.
- The production repository may consume only exact immutable evidence from the test aggregate.
- Product service conformance requires one `*-lib-core` persistence authority, API-owned product writes, bounded database-enforced web reads, isolated web-state writes, migrator-only DDL, and Shared Auth without product-domain database ownership.

See `config/repository.json`, `pins/source.json`, and `.github/workflows/e2e.yml` for the machine-enforced contract.
