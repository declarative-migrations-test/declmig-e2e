# tkda-main-server.rs role

Rust grandaddy process for Takoda browser execution. It owns language-neutral worker lifecycle, process supervision, >=20-minute run windows, retries, cancellation, heartbeats, run status, driver command correlation and bounded AI-replanning requests.

Supported worker languages: TypeScript, Rust, Go and Python. Supported browser engines initially: Selenium, Playwright and Puppeteer. The supervisor must not assume one language owns the protocol.

Reference `canonical-cloud/canonical-worker.rs`, `tkda-interfaces`, `tkda-browser-workers.ts`, `tkda-selenium-server`, `scintilla-run`, `ores-otel`, `ores-rate-limit`, `ORESoftware/ores-locks-and-leases`, and `ORESoftware/k8s-cluster` browser-runtime hardening.

The checked-in language adapters are conformance canaries; production workers may run remotely on Scintilla Run as long as they preserve the same protocol and lifecycle semantics.
