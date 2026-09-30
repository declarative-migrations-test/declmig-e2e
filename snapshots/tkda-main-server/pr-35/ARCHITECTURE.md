# tkda-main-server.rs architecture

Rust is the Takoda execution supervisor. It accepts run assignments, supervises TypeScript/Rust/Go/Python worker adapters, owns bounded retries/timeouts/cancellation, and bridges HTTP/WebSocket driver traffic into the shared worker command/event protocol.

Browser libraries remain inside worker adapters. Runtime executables are operator-configured allowlisted adapters; the API does not accept arbitrary shell commands.

## Independent execution axes

Takoda follows the authored `tkda-interfaces` contract and keeps three concerns separate:

- `execution_target = local | scintilla`: runtime/provider transport.
- `placement_preference = auto | cloud | desktop`: product placement policy.
- `execution_mode = headless | headed`: browser presentation.

The realized run snapshot records `execution_location = cloud | desktop` plus an optional `executor_id`.

Placement is fail-closed: `desktop` never silently falls back to cloud, and `cloud` never moves onto a user's laptop. `auto` may use a compatible desktop agent and otherwise falls back to the requested transport target.

Headed local execution is denied unless `TKDA_ALLOW_HEADED=true`.

A daemon-owned local supervisor sets `TKDA_EXECUTION_ROLE=desktop`. In that role the already-selected desktop placement is executed locally rather than recursively leasing another desktop agent.

## Desktop agent control plane

Desktop agents establish an outbound authenticated WebSocket at `/v1/agents/connect`. The server and daemon share protocol version 1:

- agent -> server: `hello`, `heartbeat`, `lease_accepted`, `lease_rejected`, `rpc_result`
- server -> agent: `ping`, `lease`, `rpc`

Agent selection matches browser engine, worker language, headed/headless capability, optional `preferred_agent_id`, heartbeat freshness, and declared concurrency.

When a cloud supervisor selects a desktop agent, it normalizes the leased run to `execution_target=local` and `placement_preference=desktop`. The daemon validates that policy and creates the run on its authenticated loopback supervisor. The cloud supervisor polls run state and relays only allowlisted run RPCs through the daemon; no inbound desktop port is required.

Connection IDs fence replaced sockets; lease/RPC IDs correlate responses. Desktop disconnects and rejected leases are retryable only within the original run retry/time budget.

The current `TKDA_AGENT_TOKEN` is a deployment credential. Multi-tenant production should issue per-user/per-device credentials through Shared Auth so agent ownership is bound to the authenticated principal instead of relying on one shared deployment secret.

## Shared execution core

`tkda-main-server.rs` and `tkda-desktop-daemon` are placement adapters around one execution model. The language launcher, worker command/event codec, retry classification, timeout/cancellation state machine, and cleanup rules should continue moving into `tkda-lib-core`.

AI planning is a bounded worker capability, not an authorization layer. Replanning cannot weaken target, placement, secret, network, process, or retry policy.


## Remote Scintilla trust boundary

Remote `execution_target=scintilla` launch is treated as a separate authenticated network boundary.

- `TKDA_SCINTILLA_LAUNCH_URL` must be credential-free HTTPS, except literal loopback HTTP for development.
- `TKDA_PUBLIC_BASE_URL` is a credential-free root HTTPS URL (literal loopback HTTP is development-only).
- Non-loopback launch requires `TKDA_SCINTILLA_TOKEN_FILE`; the legacy inline `TKDA_SCINTILLA_TOKEN` is rejected.
- Secret files are absolute, regular non-symlink files, bounded to 16 KiB, and mode 0600 on Unix.
- HTTP redirects are disabled.
- Launch and worker responses are streamed with a 256 KiB ceiling.
- Returned worker URLs must themselves satisfy the HTTPS/literal-loopback rule; non-loopback workers must return a bounded bearer token.
- Error response bodies are bounded and control characters are removed before diagnostic logging/propagation.

The remote launcher never accepts a repository/user payload as an arbitrary URL or executable.
