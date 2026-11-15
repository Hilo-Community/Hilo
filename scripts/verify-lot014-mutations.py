#!/usr/bin/env python3
"""Deliberate safety mutations. Run sequentially, with no other build/test process.

Every replacement asserts exactly one match, every original is backed up under a distinct
path-encoded name, and cmp verifies exact restoration even when a test fails unexpectedly.
No mutation is accepted on a compiler error, zero matched tests or a timeout.
"""
import json
import shutil
from pathlib import Path
import subprocess
import tempfile
import sys

ROOT = Path(__file__).resolve().parents[1]
BACKUPS = Path(tempfile.mkdtemp(prefix="coloride-lot014-mutations-"))
ACP = "crates/judge-orchestrator/src/acp/"
TEST = ["cargo", "test", "-p", "judge-orchestrator", "--test", "lot014_acp"]
UI = ["pnpm", "exec", "vitest", "run", "apps/web-ui/src/agent/agent.test.ts"]
MUTATIONS = [
    ("host-reader-bypasses-rope", "apps/desktop-tauri/src-tauri/src/commands/agent.rs", "let path = PathGuard::verify_within_worktree(&self.root, &self.root.join(relative))\n            .map_err(|e| e.to_string())?;", "let path = PathGuard::verify_within_worktree(&self.root, &self.root.join(relative))\n            .map_err(|e| e.to_string())?;\n        return std::fs::read_to_string(&path).map_err(|e| e.to_string());", ["cargo", "test", "-p", "coloride-desktop", "--lib", "proposal_base_reads_unsaved_rope"]),
    ("ignore-host-rejection", ACP + "session.rs", "if !allow {", "if false && !allow {", TEST + ["denied_policy_and_host_rejection"]),
    ("retain-released-terminal", ACP + "session.rs", "self.terminals.remove(id);", "let _ = id;", TEST + ["terminal_lifecycle_replays"]),
    ("swallow-sandbox-refusal", ACP + "session.rs", "launch_broker\n            .execute(&host.lease, &host.signed, &call, &[], (host.clock)())\n            .map_err(|e| AcpError::Launch(e.to_string()))?;", "let _ = launch_broker.execute(&host.lease, &host.signed, &call, &[], (host.clock)());", TEST + ["unavailable_sandbox_untrusted"]),
    ("bypass-provider-egress-refusal", ACP + "session.rs", "if let AgentNetwork::ProviderOnly { destination } = launch.network {", "if let AgentNetwork::ProviderOnly { destination } = AgentNetwork::Offline {", TEST + ["unavailable_sandbox_untrusted"]),
    ("bypass-credential-refusal", ACP + "session.rs", "match launch.credentials {", "match AgentCredentials::Unauthenticated {", TEST + ["unavailable_sandbox_untrusted"]),
    ("bypass-raw-output-budget", ACP + "protocol.rs", "remaining: limit", "remaining: u64::MAX", ["cargo", "test", "-p", "judge-orchestrator", "--lib", "malformed_envelopes"]),
    ("proposal-writes-disk", ACP + "executor.rs", "proposals.insert(call.idempotency_key.clone(), edit.clone());", "std::fs::write(self.root.join(&edit.path), &edit.new_text).unwrap();\n                proposals.insert(call.idempotency_key.clone(), edit.clone());", TEST + ["agent_write"]),
    ("base-from-disk", ACP + "executor.rs", "let text = self.reader.text(path).map_err(ExecutionError::NotStarted)?;", "let text = std::fs::read_to_string(self.root.join(path)).map_err(|e| ExecutionError::NotStarted(e.to_string()))?;", TEST + ["agent_write"]),
    ("write-without-consent", ACP + "session.rs", "preconditions: if approval {", "preconditions: if false && approval {", TEST + ["agent_write"]),
    ("bypass-tool-policy", "crates/judge-orchestrator/src/policy/mod.rs", "if !schema::valid(spec, call) {", "if false && !schema::valid(spec, call) {", TEST + ["traversal_absolute"]),
    ("bypass-sandbox-self-test", "crates/sandbox-runner/src/runner.rs", "if let Err(error) = engine.self_test() {", "if let Err(error) = Ok::<(), SandboxError>(()) {", ["cargo", "test", "-p", "sandbox-runner", "--test", "runner", "fail_closed_a_failed_self_test"]),
    ("inherit-host-secrets", "crates/sandbox-runner/src/seatbelt.rs", ".env_clear()", "", TEST + ["real_environment"]),
    ("bypass-path-traversal", "crates/judge-orchestrator/src/policy/paths.rs", '".." => return None,', '".." => parts.push(".."),', TEST + ["traversal_absolute"]),
    ("bypass-forbidden-case", "crates/judge-orchestrator/src/policy/paths.rs", ".any(|root| under(&folded, &root.to_lowercase()))", ".any(|root| false && under(&folded, &root.to_lowercase()))", TEST + ["traversal_absolute"]),
    ("accept-malformed-json", ACP + "protocol.rs", "let value: Value = serde_json::from_slice(&bytes)?;", 'let value: Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({"jsonrpc":"2.0","method":"session/update"}));', ["cargo", "test", "-p", "judge-orchestrator", "--lib", "malformed_envelopes"]),
    ("remove-message-size-limit", ACP + "protocol.rs", "if bytes.len() + count > MAX_MESSAGE_BYTES {", "if false && bytes.len() + count > MAX_MESSAGE_BYTES {", ["cargo", "test", "-p", "judge-orchestrator", "--lib", "malformed_envelopes"]),
    ("remove-pending-limit", ACP + "session.rs", "if self.pending.len() >= MAX_PENDING_REQUESTS {", "if false && self.pending.len() >= MAX_PENDING_REQUESTS {", TEST + ["malformed_oversized"]),
    ("accept-unknown-method", ACP + "session.rs", '_ => return Err("Unknown ACP method".into()),', '_ => ("read_workspace", ToolCapability::ReadWorkspace, BTreeMap::from([("path".into(), ToolArgValue::Text("src/file.rs".into()))]), false),', TEST + ["malformed_oversized"]),
    ("remove-cancellation-kill", ACP + "session.rs", "pub fn cancel(&self) {\n        self.executor.stop();\n    }", "pub fn cancel(&self) {}", TEST + ["cancellation_kills"]),
    ("remove-workspace-trust", "crates/host-core/src/trust.rs", "if !self.inner.trusted.load(Ordering::SeqCst) {", "if false && !self.inner.trusted.load(Ordering::SeqCst) {", ["cargo", "test", "-p", "coloride-desktop", "--lib", "untrusted_host_cannot_access_agents"]),
    ("writable-agent-worktree", "crates/sandbox-runner/src/seatbelt.rs", "let writable = if spec.worktree_read_only {", "let writable = if false && spec.worktree_read_only {", TEST + ["real_environment"]),
    ("bypass-mediated-reads", ACP + "session.rs", "spec.mediate_worktree_reads = true;", "spec.mediate_worktree_reads = false;", TEST + ["real_environment"]),
    ("unsafe-markdown", "apps/web-ui/src/ui/stream-renderer.ts", 'return `<p>${escapeHtml(trimmed)}</p>`;', 'return `<p>${trimmed}</p>`;', UI + ["-t", "batches tokens"]),
    ("unsafe-ui-location", "apps/web-ui/src/agent/store.ts", 'path.split("/").includes("..")', 'false', UI + ["-t", "merges plans"]),
    ("editable-proposal", "apps/web-ui/src/agent/panel.ts", 'const diff = doc.createElement("pre");', 'const diff = doc.createElement("pre"); diff.contentEditable = "true";', UI + ["-t", "sends prompts"]),
    ("editor-dom-replaced", "apps/web-ui/src/agent/panel.ts", "this.output.replaceChildren(...content);", 'this.output.replaceChildren(...content); doc.querySelector(".cm-editor")?.remove();', UI + ["-t", "streaming in the mounted"]),
]

results = []
print(f"Backups and logs: {BACKUPS}", flush=True)
UI_ROOT = BACKUPS / "isolated-ui-workspace"
UI_ROOT.mkdir()
for top in ("apps", "packages"):
    shutil.copytree(ROOT / top, UI_ROOT / top, ignore=shutil.ignore_patterns("node_modules", "target", "dist"))
for config in ("package.json", "pnpm-workspace.yaml", "tsconfig.json", "tsconfig.base.json"):
    if (ROOT / config).exists(): shutil.copy2(ROOT / config, UI_ROOT / config)
(UI_ROOT / "node_modules").symlink_to(ROOT / "node_modules", target_is_directory=True)
for index, (name, relative, before, after, command) in enumerate(MUTATIONS):
    if len(sys.argv) > 1 and name not in sys.argv[1:]: continue
    # Keep deliberate UI corruption away from any dev server already watching the worktree.
    cwd = UI_ROOT if relative.startswith("apps/web-ui/") else ROOT
    path = cwd / relative
    original = path.read_bytes()
    needle = before.encode()
    assert original.count(needle) == 1, (name, original.count(needle))
    backup = BACKUPS / f"{index:02d}--{relative.replace('/', '__')}--{name}.bak"
    backup.write_bytes(original)
    try:
        path.write_bytes(original.replace(needle, after.encode()))
        run = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=180)
        output = run.stdout + run.stderr
        (BACKUPS / f"{index:02d}--{name}.log").write_text(output)
        assert run.returncode != 0, f"mutation survived: {name}"
        assert "test result: FAILED" in output or "AssertionError" in output or "FAIL " in output, f"no failing assertion: {name}\n{output[-3000:]}"
        assert "could not compile" not in output and "error[E" not in output, f"compiler rejection is not evidence: {name}"
        results.append({"mutation": name, "result": "assertion failed", "command": command, "backup": str(backup)})
        print(f"Detected {name}", flush=True)
    finally:
        path.write_bytes(original)
        subprocess.run(["cmp", str(path), str(backup)], check=True)
        assert path.read_bytes() == original

(BACKUPS / "results.json").write_text(json.dumps(results, indent=2) + "\n")
print(f"Verified {len(results)} mutations; all files restored byte-for-byte", flush=True)
