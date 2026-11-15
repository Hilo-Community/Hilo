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

- Paths are absolute, as ACP specifies. `session/new` gives the agent the canonical worktree
  root as `cwd`; a path under it becomes the worktree-relative path the LOT-013 policy checks,
  and any other path reaches the policy unchanged and is refused. Locations in session updates
  are rewritten the same way, so the panel can open them.
- `fs/read_text_file` runs `read_workspace`. The trusted desktop reader takes the live Rope of
  an open document, including unsaved text, otherwise the file on disk; reading never opens a
  document in the editor. Lexical LOT-013 policy and canonical sandbox path checks both apply, including forbidden-path
  checks after symlink resolution.
- `fs/write_text_file` runs the proposal-only executor for `write_worktree`, after exact-call
  Host approval. It stores one `ProposedEdit` with path, BLAKE3 of the current host-core text,
  old text and new text. It never invokes a document or filesystem write API. Applying is LOT-015.
- `terminal/create` runs `run_sandbox_command` in a separate read-only task sandbox after approval.
  Creation responds after completion. `output`, `wait_for_exit`, `kill` and `release` replay the
  original brokered call to recheck policy and return its bounded cached result; they do not
  start another process. `kill` is a no-op for an already completed terminal. A verbose command
  is not killed for its output: the end is kept and `truncated` is set, as ACP specifies. This
  is intentionally not a live PTY or interactive terminal.
- Cancelling (`AcpSessionSupervisor::cancel`, the panel's Cancel) ends the current turn: running
  commands are killed, pending requests answered as cancelled and `session/cancel` sent. The
  agent and the session stay. `stop` ends the session and kills every process.
- `session/request_permission` only asks the Host; selecting an agent option grants no signed
  Coloride authority. A subsequent fs/terminal request must still pass policy independently.
  `RequiresApproval` retains the exact call. Only `HostAuthority` signs its bound, expiring
  approval, once. Native dialogs display the exact pending operation; the webview cannot approve
  a different operation by supplying an arbitrary digest or signing key.
- `_coloride/query_local_mcp` runs `query_local_mcp`. The desktop adapter reuses LOT-009's
  `LocalMcpServer`, ACL/secret scanner, `LocalOnly` export routing and live host-core overlays.
  No MCP result can sign a permission or change task policy.

## Explicit specification conflicts and limits

1. ACP filesystem writes normally mean a file was written. Here the response follows policy
   approval and proposal recording only. Client capability metadata declares
   `writesAreProposals`; no application is implied (that is LOT-015).
2. `terminal/create` needs an absolute `command`: the sandbox never searches `PATH`, so an agent
   asking for `npm` is refused until the Host resolves programs itself.
3. `session/new.mcpServers` is empty. A normal stdio MCP registration would let the agent spawn
   a server outside the ToolBroker mediation path. The brokered Coloride extension is available;
   standard-agent MCP registration needs a separately supervised broker relay. It is not claimed
   interoperable with arbitrary unmodified agents.
4. Files larger than 64 KiB (one journaled summary) cannot be read or proposed; they are refused,
   never cut.
5. macOS and Linux provide the read-only task mode. Windows refuses it explicitly until its
   AppContainer backend supports read-only worktree ACLs. Native Linux/Windows behavior needs CI.
6. The runner's Unix process-group supervision is inherited. Its documented limitation for a
   descendant deliberately calling `setsid` remains relevant on macOS; tests verify real ordinary
   child trees and abrupt-host supervision, not arbitrary hostile process-group escape resistance.
7. Proposals/conversation/terminal caches are session-local. Durable broker records survive a
   restart; restoring an ACP conversation or pending proposal after a restart is not implemented.
   Unknown external outcomes retain their recorded intent and require reconciliation; no retry.

## Bounds and UI

Messages are at most 1 MiB (a 64 KiB file fits even if every byte is escaped), pending incoming
and outgoing requests at most 32, calls/proposals at most 512, file/proposal/MCP text at most
64 KiB, prompts at most 24 KiB and the kept terminal output 16 KiB (its end), cut further if its
JSON would not fit a journaled summary. Session stdout and
stderr have the Host output ceiling; the runner enforces lifetime and requested resource ceilings.
Malformed/oversized envelopes close the session. Unknown methods return errors. The handshake
has a ten-second deadline. The command environment accepts no agent-supplied variables.

The agent panel has an isolated atomic store and 16 ms `requestAnimationFrame` batching. It
renders escaped Markdown, plan checklists, typed tool statuses, location buttons, read-only
line diffs of proposals (only the changes and three lines of context around each), pending
approvals, denial reasons, prompt and cancel controls. It never owns the editor or publishes streaming updates into Zone 4. Transcript retention is bounded.
Starting another session fences late events, stops the previous handle and resets the store.

## Tests and deliberate mutations

`tests/lot014_acp.rs` launches the compiled `coloride-acp-test-agent` inside the real native
sandbox. No provider agent/account is needed. It covers exact Rope diff bases, no writes,
policy and Host rejection, path/case refusals, absolute ACP paths inside the worktree,
malformed/oversized messages, unknown methods, pending overflow, real child environment
isolation, direct write prevention, turn cancellation that keeps the agent, session stop that
kills every process, durable intent settlement, terminal lifecycle replay, verbose terminal
output and fail-closed launch. `acp::protocol::tests` covers malformed envelopes and
`acp::executor::tests` the terminal result bound; desktop tests cover trust, the live Rope,
closed files read from disk without opening a document, and launch argument bounds.
`apps/web-ui/src/agent/agent.test.ts` covers batching, escaped Markdown, plans/tool updates,
unsafe locations, read-only diffs, prompt/permission/cancel actions and actual singleton
CodeMirror identity/container/mount count while streaming in the mounted cockpit;
`diff.test.ts` covers the proposal line diff.

Each mutation below made its safety test fail with an assertion (not a compile error), and
was restored exactly (checked with `cmp`) before the final checks:

- Reintroduce a proposal disk write; read the base from disk; bypass the desktop Rope reader;
  remove write consent; ignore a Host rejection.
- Bypass tool policy; bypass the sandbox self-test; inherit the Host environment; allow direct
  agent writes; bypass mediated source reads; swallow a sandbox refusal; bypass the
  provider-egress and, separately, the credential refusal; remove workspace trust.
- Permit traversal; remove case-insensitive forbidden-path matching; stop translating absolute
  ACP paths.
- Accept malformed JSON; remove the message-size, raw-output and pending-count limits; accept
  an unknown method; retain a released terminal.
- Omit the cancellation kill; make a turn cancellation kill the agent; stop flagging dropped
  terminal output; stop cutting a terminal result to fit a journaled summary.
- Inject raw Markdown; allow unsafe UI locations; make the diff editable; remove the editor
  DOM during a panel repaint.

Optional vector/model inference tests are outside this scoped task.
