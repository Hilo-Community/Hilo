# LOT-013 governance core

The scoped LOT-014 ACP supervisor is documented in [acp/README.md](src/acp/README.md).
It adds no dependency: the bounded JSON-RPC client reuses the existing `serde_json` stack.
The current official Rust SDK's workspace declares Rust 1.88; this workspace declares 1.80.
We retain synchronous supervised pipes and explicit framing/request bounds instead of adding
that SDK and its transport/schema dependency tree. See the linked notes for protocol conflicts,
platform refusals, mutation evidence and the deliberately deferred provider/authentication work.

Implementation notes and test evidence; LOT-013 acceptance remains a human signoff.

## Host boundary

The trusted Host owns `HostAuthority`, creates tasks, signs contracts, grants leases,
configures executors/worktree roots and supplies time. Never expose these operations
as model, MCP or extension tools. Keys are generated from OS entropy and cannot be
serialized or formatted. A restarted Host uses a new authority and must authenticate
its task/approval sources before signing again. Journals are trusted Host storage;
keep them outside the writable task worktree.

`ToolBroker::execute` authenticates policy, checks idempotency (a settled call returns
its stored outcome without reserving anything, so a replay never pauses a task),
reserves budget, flushes an intent, invokes the trusted executor, and atomically
records both outcome and spending. The recorded summary is bounded to 64 KiB (both
ends kept, plus the BLAKE3 of the full output), so a large output cannot exceed a
journal record. An executor that proves nothing ran (`ExecutionError::NotStarted`,
e.g. the sandbox refused the request before spawning) has its intent closed as failed
at no charge. Other unknown outcomes (errors or panics) leave intents pending. Lease
takeover moves such tasks to `NeedsReconciliation`; only the existing Host human
reconciliation flow settles them. The broker never retries an external action.
An unresolved action's unknown consumption must be assessed during reconciliation;
only committed spending is replayed automatically. Reservations are intentionally
released on cancellation and do not survive a process crash.

Time is milliseconds throughout. The broker receives entry time and an injected Host
clock for completion, so an action finishing after lease expiry cannot publish a
result. Repository logic never reads the clock itself, and `BudgetLedger` holds none:
every reservation, commit and correction cycle takes the Host time, so a ledger kept
past its lease is refused.

Forbidden paths are matched ignoring case (macOS and Windows file systems are
case-insensitive by default); allowed paths are matched exactly, which can only deny
more.

## Tool schemas

Unknown tool names, extra/missing arguments, mismatched or empty capabilities and
unsupported argument types are refused. String payloads convey data, not authority.

| Tool | Required capability | Arguments |
| --- | --- | --- |
| `read_workspace` | `ReadWorkspace` | `path: Text` |
| `write_worktree` | `WriteWorktree` | `path: Text`, `content: Text` |
| `run_sandbox_command` | `RunSandboxCommand` | `program: Text`, `cwd: Text`, `args: StringList` |
| `query_local_mcp` | `QueryLocalMcp` | `request: Text` |
| `network_egress` | `NetworkEgress` | `request: Text` |
| `database_read` | `DatabaseRead` | `request: Text` |
| `database_migrate` | `DatabaseMigrate` | `target_env: Text`, `request: Text` |
| `deploy_preview` | `DeployPreview` | `target_env: Text`, `request: Text` |
| `deploy_production` | `DeployProduction` | `target_env: Text`, `request: Text` |

Sandbox commands alone may additionally request `NetworkEgress` (second capability).
Network still requires signed policy permission and a Host-configured loopback proxy;
without all three it remains denied. Native runner capability checks remain fail-closed.

Paths and cwd must be relative worktree paths covered by allowed paths and excluded
from forbidden paths. `.` denotes the worktree root; traversal and Windows roots are
refused on every OS. The program is the deliberate absolute-path exception. Arguments
are opaque argv data; the sandbox, not lexical inspection of shell text, contains the
process. The sandbox executor also resolves cwd symlinks through `PathGuard`.

Target strings are `local_worktree`, `local_oci_sandbox`, `staging`, `production`, and
`sanitized_database_clone`. Preview requires staging, production deploy requires
production. Migration/deploy approvals use the LOT-002 digest helper over the entire
serialized call and task plan, including signed policy and budgets.

`SandboxToolExecutor` is the provided execution adapter; other authorized tool schemas
need trusted Host adapters implementing `ToolExecutor`. Estimates must be pure maxima,
and adapters must enforce their reservation while streaming. The native adapter charges
the full configured CPU allowance rounded up to seconds; it does not claim measured
aggregate process-tree CPU telemetry. Native resource enforcement remains LOT-012's
per-OS capability contract.

## Acceptance evidence

All names below are executable tests, not a universal proof of immunity to arbitrary
future adapters or platform changes.

| Criterion | Test evidence |
| --- | --- |
| Durable lifecycle, generations and crash recovery | `lot013_state_machine.rs`, plus journal and transition unit tests |
| No blind retries after external action | `crash_after_external_action_and_expired_worker_rejection_has_zero_blind_retries` |
| Expired worker cannot publish after a blocking call | `completion_after_expiry_cannot_publish_or_charge_as_a_live_worker` |
| Replay stores outcomes and binds keys to call digests | `resolved_call_is_replayed_without_execution_or_spending_and_key_binds_digest` |
| Injection cannot grant privilege | `prompt_injection_via_file_or_mcp_never_grants_escalation` |
| Host MAC and exact approval binding | `only_host_authenticated_unmodified_contracts_authorize`, `privileged_approvals_bind_exact_call_plan_kind_target_and_expiry` |
| Two default retries and signed policy override | `automatic_retries_default_to_two_and_only_host_policy_can_raise_limit` plus broker equality of signed/persisted specs |
| Reservation and repeated failure stop execution | `budget_and_loop_circuit_breaker_halts_on_actual_overrun_or_second_identical_failure`, `broker_budget_overrun_repeated_failure_and_retry_limit_stop_execution`, `exhausted_upfront_budget_executes_nothing` |
| Cancellation cleanup and persistent spending | `cancellation_error_and_panic_release_reservations_and_commit_releases_the_rest`, `ledgers_share_reservations_and_spending_survives_restart` |
| Native execution through policy and budget | `real_native_sandbox_runs_only_after_policy_budget_and_durable_intent` (no skip when sandbox unavailable) |

## Deliberate mutations exercised

Each mutation below produced a failing assertion in the corresponding safety test,
then was restored before the final checks:

- Bypassed task MAC verification.
- Bypassed tool schema/capability correspondence (injection test).
- Bypassed approval MAC verification and, separately, action binding.
- Removed task-state, autonomy, trust, network-policy and path-scope gates separately.
- Removed generation fencing and, separately, lease-expiry fencing.
- Removed upfront reservation limits, cancellation `Drop` cleanup, repeated-failure
  stopping, retry-cap stopping and replay of committed token spending separately.
- Disabled cached outcome lookup, omitted durable intent before execution, and
  suppressed takeover reconciliation separately.
- Removed the sandbox network capability gate and cwd symlink guard separately.
- Disabled actual-over-reservation stopping at broker settlement.

## Verification boundaries

Local mandatory workspace checks and the native broker integration run on macOS.
Linux cross-target clippy is attempted for each part but is blocked before checking
this crate: `libz-sys` cannot find `x86_64-linux-gnu-gcc` (and cross pkg-config is not
configured). Linux and Windows execution require CI evidence; no remote CI or push
is performed here. Existing local sandbox-runner changes are prerequisites left
outside these commits. The desktop model/agent workflow is not wired by this lot.
