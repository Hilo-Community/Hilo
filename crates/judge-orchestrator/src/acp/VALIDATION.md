# Scoped LOT-014 validation — 2026-11-15

Local machine: macOS / aarch64. Branch: `feat/lot-014`, created from fetched `origin/main`
at `0ad3816`. No push or remote CI was performed.

The following chain passed, and was repeated after restoring the deliberate mutations:

```sh
pnpm lint && pnpm typecheck && pnpm test && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Results: 221 JavaScript tests and 247 Rust tests passed; zero failures or ignored Rust tests.
The default suite does not enable the optional context-engine vector feature. Relevant new tests:
eight fake-agent/native-sandbox integration tests, one protocol safety unit test, two desktop
Host unit tests and four agent-panel/store Vitest tests. Each part shares this full-workspace gate.

Additional successful commands:

```sh
cargo test -p judge-orchestrator --test lot014_acp -- --test-threads=1
cargo test -p judge-orchestrator --lib acp::protocol
cargo test -p coloride-desktop --lib commands::agent
pnpm exec vitest run apps/web-ui/src/agent/agent.test.ts
pnpm build
cargo clippy -p sandbox-runner -p sandbox-windows --target x86_64-pc-windows-msvc -- -D warnings
cargo clippy -p sandbox-runner -p sandbox-windows --target x86_64-unknown-linux-gnu -- -D warnings
git diff --check
```

`pnpm build` succeeded with Vite's non-blocking warning about the main chunk exceeding 500 kB
(approximately 1.38 MB minified). The cross-target commands check the sandbox crates, not a
cross build of the entire Tauri/C-dependency workspace. Linux and Windows execution still need
their native CI machines; Windows agent launch intentionally refuses read-only ACL mode.

Mutation evidence (all failing assertions, no compiler failures counted):

```sh
python3 scripts/verify-lot014-mutations.py
python3 scripts/verify-lot014-mutations.py bypass-raw-output-budget
python3 scripts/verify-lot014-mutations.py host-reader-bypasses-rope ignore-host-rejection retain-released-terminal swallow-sandbox-refusal bypass-provider-egress-refusal bypass-credential-refusal
```

The first command ran the original 19, then expanded 20/21 mutation sets as the corresponding
guards were added. The final targeted command covered the remaining six desktop/protocol
boundaries: **27 distinct mutations detected** in total. The current script includes all 27 in
its default run. Every mutation restored its exact backup and ran `cmp`; final restoration also
compared the supervisor with the last path-encoded backup and checked the Git diff of preexisting
policy, trust, runner and Markdown files (all unchanged).

Final 21-mutation evidence directory:
`/var/folders/md/7bv7rsmj59d18ycjwczdwgrw0000gn/T/coloride-lot014-mutations-tl_vn17q`.
Final six targeted mutations:
`/var/folders/md/7bv7rsmj59d18ycjwczdwgrw0000gn/T/coloride-lot014-mutations-cxig4bd0`.
Each contains path-encoded `.bak` files, per-test logs and `results.json`.

Visual review: the actual panel component was rendered in Chrome with synthetic conversation,
checklist, proposal diff and permission events; its visible layout and accessibility tree were
inspected. The temporary preview file/server were removed afterward. This is not a real provider
agent or an end-to-end Tauri native-dialog/agent test. No provider credential, network fallback,
LLM latency or local model runtime was exercised or claimed. See `README.md` for the remaining
protocol/platform conflicts, including standard MCP registration and macOS process-group escape.
