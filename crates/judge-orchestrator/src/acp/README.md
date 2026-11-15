# LOT-014: scoped ACP bridge and agent panel

This task covers the ACP bridge, Tauri Host commands and generic agent panel. It does not
implement a Model Gateway, keyring, model registry, local inference runtime or changeset
application. Those parts of LOT-014 / LOT-015 remain pending. A successful tool call is never
presented as `ValidatedGreen` evidence.

## Transport and dependency decision

The client implements the bounded ACP v1 subset over newline-delimited UTF-8 JSON-RPC.
No new Cargo or JavaScript dependencies were introduced, so there are no new version ranges.
The existing workspace dependency versions were preserved. The official Rust SDK was reviewed
through its [documentation](https://agentclientprotocol.com/libraries/rust) and
[workspace manifest](https://github.com/agentclientprotocol/rust-sdk/blob/main/Cargo.toml):
the current manifest uses edition 2024 / Rust 1.88, above Coloride's declared Rust 1.80, and
introduces a separate protocol schema/transport stack. Retaining the existing synchronous
`SandboxedChild` pipes also makes the byte, request and lifetime boundaries explicit. This is
a constrained client, not a claim of implementing every ACP capability.

Sources checked: [initialization](https://agentclientprotocol.com/protocol/v1/initialization),
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup),
[prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn),
[filesystem](https://agentclientprotocol.com/protocol/v1/file-system),
[terminals](https://agentclientprotocol.com/protocol/v1/terminals),
[tool permissions](https://agentclientprotocol.com/protocol/v1/tool-calls) and
[stdio framing](https://agentclientprotocol.com/protocol/v1/transports).

## Host boundary

Only trusted Host code constructs `AcpHost`, the signed LOT-013 contract, lease, sandbox runner,
workspace reader, clock and event sink. Start itself passes through `ToolBroker`, so launch
has an authenticated policy decision, upfront budget reservation, durable intent and outcome.
There is no raw process-launch fallback. The Host creates a LOT-011 checkpoint from host-core
buffers and provisions a separate CoW task worktree; unsupported filesystems fail closed.
The source workspace's trust gate remains authoritative across a switch or revocation.

The complete agent environment is empty; sandbox-runner supplies only private temporary-directory
variables. No inherited BYOK variables, credential fields, `HOME`, writable source workspace,
external mount or user directory is accepted by the supervisor. Requested CPU/memory/process
limits are passed unchanged to sandbox-runner; a backend that cannot enforce them refuses.
The agent's source reads are also mediated: macOS denies worktree file data except the exact
executable and scratch directory; Linux exposes only those two paths inside an empty task root.
The kernel prevents reading source directly to bypass a denied ACP request. Approved broker
terminal commands can read the task snapshot but cannot write it. Agents loading their own code
modules from the task worktree need a standalone/system-installed executable instead.

`AgentNetwork::Offline` is supported. `ProviderOnly { destination }` always refuses before spawn:
`LocalProxyOnly` restricts an OS endpoint but does not itself authenticate or filter provider
destinations. No destination-filtering Host proxy exists yet. macOS cannot silently use its
proxy capability to open provider egress; Linux and Windows cannot silently open the network.

`AgentCredentials::Unauthenticated` is explicit. `HostMediated` refuses with an authentication
error; no agent login, key or home mount is invented. Choosing a provider authentication mechanism
is still the user's decision. Real provider agents are not claimed operational in this task.

## Brokered operations

- `fs/read_text_file` runs `read_workspace`. The trusted desktop reader looks up the live Rope
  first, including unsaved text; unopened source files enter host-core before becoming a diff base.
  Lexical LOT-013 policy and canonical sandbox path checks both apply, including forbidden-path
  checks after symlink resolution.
- `fs/write_text_file` runs the proposal-only executor for `write_worktree`, after exact-call
  Host approval. It stores one `ProposedEdit` with path, BLAKE3 of the current host-core text,
  old text and new text. It never invokes a document or filesystem write API. Applying is LOT-015.
- `terminal/create` runs `run_sandbox_command` in a separate read-only task sandbox after approval.
  Creation responds after completion. `output`, `wait_for_exit`, `kill` and `release` replay the
  original brokered call to recheck policy and return its bounded cached result; they do not
  start another process. `kill` is a no-op for an already completed terminal. Session cancellation
  kills an in-flight command. This is intentionally not a live PTY or interactive terminal.
- `session/request_permission` only asks the Host; selecting an agent option grants no signed
  Coloride authority. A subsequent fs/terminal request must still pass policy independently.
  `RequiresApproval` retains the exact call. Only `HostAuthority` signs its bound, expiring
  approval, once. Native dialogs display the exact pending operation; the webview cannot approve
  a different operation by supplying an arbitrary digest or signing key.
- `_coloride/query_local_mcp` runs `query_local_mcp`. The desktop adapter reuses LOT-009's
  `LocalMcpServer`, ACL/secret scanner, `LocalOnly` export routing and live host-core overlays.
  No MCP result can sign a permission or change task policy.

## Explicit specification conflicts and limits

1. ACP v1 normally uses absolute filesystem paths. This task explicitly requires rejecting
   them. Filesystem/cwd tool arguments therefore require portable relative paths; only
   `session/new.cwd` and an executable's `program` use absolute paths. Agents expecting standard
   absolute filesystem callbacks need a relative-path adapter. No permissive translation is hidden.
2. ACP filesystem writes normally mean a file was written. Here the response follows policy
   approval and proposal recording only, as explicitly requested. Client capability metadata
   declares `relativePathsOnly` and `writesAreProposals`; no application is implied.
3. `session/new.mcpServers` is empty. A normal stdio MCP registration would let the agent spawn
   a server outside the ToolBroker mediation path. The brokered Coloride extension is available;
   standard-agent MCP registration needs a separately supervised broker relay. It is not claimed
   interoperable with arbitrary unmodified agents.
4. macOS and Linux provide the read-only task mode. Windows refuses it explicitly until its
   AppContainer backend supports read-only worktree ACLs. Native Linux/Windows behavior needs CI.
5. The runner's Unix process-group supervision is inherited. Its documented limitation for a
   descendant deliberately calling `setsid` remains relevant on macOS; tests verify real ordinary
   child trees and abrupt-host supervision, not arbitrary hostile process-group escape resistance.
6. Proposals/conversation/terminal caches are session-local. Durable broker records survive a
   restart; restoring an ACP conversation or pending proposal after a restart is not implemented.
   Unknown external outcomes retain their recorded intent and require reconciliation; no retry.

## Bounds and UI

Messages are at most 256 KiB, pending incoming and outgoing requests at most 32, calls/proposals
at most 512, file/proposal text and combined terminal output at most 24 KiB. Session stdout and
stderr have the Host output ceiling; the runner enforces lifetime and requested resource ceilings.
Malformed/oversized envelopes close the session. Unknown methods return errors. The handshake
has a ten-second deadline. The command environment accepts no agent-supplied variables.

The agent panel has an isolated atomic store and 16 ms `requestAnimationFrame` batching. It
renders escaped Markdown, plan checklists, typed tool statuses, location buttons, read-only
proposal diffs, pending approvals, denial reasons, prompt and cancel controls. It never owns
the editor or publishes streaming updates into Zone 4. Transcript retention is bounded.
Starting another session fences late events, stops the previous handle and resets the store.

## Tests and deliberate mutations

`tests/lot014_acp.rs` launches the compiled `coloride-acp-test-agent` inside the real native
sandbox. No provider agent/account is needed. It covers exact Rope diff bases, no writes,
policy and Host rejection, path/case refusals, malformed/oversized messages, unknown methods,
pending overflow, real child environment isolation, direct write prevention, cancellation of
actual children, durable intent settlement, terminal lifecycle replay and fail-closed launch.
`acp::protocol::tests` covers malformed envelopes; desktop tests cover trust and live Rope lookup.
`apps/web-ui/src/agent/agent.test.ts` covers batching, escaped Markdown, plans/tool updates,
unsafe locations, read-only diffs, prompt/permission/cancel actions and actual singleton
CodeMirror identity/container/mount count while streaming in the mounted cockpit.

Run `python3 scripts/verify-lot014-mutations.py` with no concurrent build or tests. Each mutation
asserts one exact source match, stores a distinct path-encoded backup, requires a failing test
assertion (compiler errors and timeouts are rejected), restores in `finally` and verifies with
`cmp`. Logs and `results.json` are retained in the printed temporary directory.

Mutations: reintroduce a proposal disk write; read the base from disk; remove write consent;
bypass tool policy; bypass sandbox self-test; inherit Host environment; permit traversal;
remove case-insensitive forbidden-path matching; accept malformed JSON; remove message-size
and pending-count limits; accept an unknown method; omit cancellation kill; remove workspace
trust; allow direct agent writes; inject raw Markdown; allow unsafe UI locations; make the
diff editable; remove the editor DOM during panel repaint; bypass mediated source reads.
The raw-output budget is also mutated independently; JSON whitespace counts against that ceiling.
Additional targeted mutations bypass the desktop Rope reader, ignore Host rejection, retain a
released terminal, swallow sandbox refusal, bypass provider-egress refusal and bypass credential
refusal. This exercises each new safety test directly, including the desktop adapters.

The exact validation commands/results and cross-target limits are recorded in
[VALIDATION.md](VALIDATION.md) and the final Spanish task report. Optional vector/model inference
tests are outside this scoped task.
