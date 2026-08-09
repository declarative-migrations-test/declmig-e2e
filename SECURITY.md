# Security policy

## Reporting

Report suspected vulnerabilities through GitHub private vulnerability reporting for the owning organization when available. Do not open a public issue containing credentials, private database URLs, exploit details for an unpatched vulnerability, or sensitive evidence artifacts.

## Test-target policy

- Pull-request workflows are secretless and must not perform destructive network writes.
- `declarative-migrations-test/declmig-e2e` may perform destructive tests only against ephemeral or explicitly disposable infrastructure.
- Production credentials, production databases, and production object stores are forbidden in the test aggregate.
- `declarative-migrations/declmig-e2e` is a promotion verifier and may not own destructive targets.
- Database URLs and cloud credentials belong in protected GitHub environments; they must never be committed or written into uploaded evidence.

## Supply-chain policy

- Pin source repositories and GitHub Actions by full commit SHA.
- Verify downloaded archives before execution or extraction.
- Do not use `curl | sh`, unreviewed installer pipes, mutable `latest` selectors, or unverified workflow artifacts in required certification lanes.
- Evidence must record exact source/workflow identities and SHA-256 artifact digests.

## Service/data boundary

A passing product conformance lane must not grant broader runtime authority:

- API is the request-serving product writer and has no broad DDL.
- Web product access is bounded read-only; product mutations traverse the API.
- Web-owned state is isolated from the product schema.
- Product DDL belongs only to the serialized migrator identity/job.
- Shared Auth has no product-domain database ownership.
