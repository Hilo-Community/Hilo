# LOT-015: the deterministic Judge

An agent's file writes are proposals (LOT-014). When its turn ends, the Host turns them into a
`ChangeSet`, compiles it against the live text, verifies the result in the OS sandbox, sends a
`🔴` result back to the agent at most twice, and shows the final result for review.

## Pieces

- `compiler.rs`: `AstChangeSetCompiler`. Each operation names its target symbol and carries the
  BLAKE3 hash of what it expects there. The target is looked up in the **current** text, so
  an operation follows its code when unrelated edits moved it (`Anchoring::Relocated`). When
  the target itself changed, the hash no longer matches and the result is a conflict
  (`CompileError::is_conflict`). Offsets are computed in UTF-8 and converted to UTF-16 with
  host-core's converter only. `result_digest` covers every `(path, result hash)`.
- `proposal.rs`: an ACP proposal (whole old and new file) becomes one `SymbolSearchReplace` per
  changed region, anchored to the top-level symbol around it, with enough context lines to be
  unique there.
- `pipeline.rs`: `JudgePipeline`. Invisible checkpoint (LOT-011), Copy-on-Write worktree, the
  snapshot written there (never in the user's workspace), then in the native sandbox: impacted
  tests (first verdict), Biome, the TypeScript checker, the whole suite. The `Evidence` names
  every command, its exit code and diagnostics, and the hash of each tested file. The Host
  signs it (`HostAuthority::sign_evidence`); accepting checks the signature.
- `correction.rs`: at most `MAX_AUTO_RETRIES` (2) automatic corrections, and an immediate
  stop when an attempt fails exactly like the previous one (same failure signature).
- `review.rs`: accepting recompiles against the text as it is now. A target the user edited
  is a conflict, never applied. A result that is byte for byte the tested content is
  `🟢 ValidatedGreen`; a change rebased over user edits is `🟠` until verified again.
- `reverify.rs`: an edited `🟢` range is `🟠` synchronously (host-core); the Judge verifies it
  again after 1.5 s idle or on save, and drops the result if the text moved meanwhile.

The desktop (`apps/desktop-tauri/src-tauri/src/commands/judge.rs`) refuses any agent-origin
`EditorTransaction` that is not the planned accept of that document, from the planned
revision, producing exactly the planned content.

## Decisions and deviations from the spec

1. **Syntax tree.** LOT-007's tree is line-granular (one node per line), not Tree-sitter. A
   symbol's block is found by a lexical scan from its declaration line (strings, template
   literals and comments skipped). A declaration the line classifier takes for a syntax error
   (`function f(` with the parameters on the next lines) is found from the text.
2. **`vtsls` in the Judge.** The sandboxed check runs `tsc --noEmit -p .` (the same TypeScript
   checker `vtsls` wraps). Its errors count as `vtsls_error_count`; their source is `Compiler`.
3. **Commands outside the ToolBroker.** The Judge's commands are the Host's (Vitest, Biome,
   TypeScript of a trusted workspace, resolved by the Host, fixed arguments). No model output
   chooses a program or an argument. They run only through `sandbox-runner` (fail closed, no
   network, no host environment) and only for a workspace the user trusted (AGENTS.md rules
   5 and 6). The agent's code runs only inside that sandbox.
4. **Vitest `--pool=threads`.** The sandbox forbids signalling other processes, so a `forks`
   pool cannot stop its workers and waits ~10 s for its own timeout. Worker threads avoid it.
5. **Retries.** "2 retries" is read as the French spec says: up to 2 correction cycles (3
   attempts), stopping at once when the same failure repeats.
6. **Preview Service.** LOT-016 adds runtime evidence; until then `preview_report` is empty
   (`inspected_url` is empty) and counts nothing.
7. **Multi-file evidence.** `tested_snapshot_hash` is the file's content hash for one file and
   `result_digest` for several; each file's hash is a `tested-file` artifact, checked per file.

## Limits

- ACP reads do not show the agent its own pending proposals (LOT-014): an agent that re-reads
  a file it proposed sees the original text.
- A proposal whose target the user edited before the turn ended cannot be verified: it is
  reported as an error, not as a review.
- Only JavaScript/TypeScript tooling is planned (`node_verification_plan`). Without Node.js or
  without the workspace's tools nothing is checked and the change is `🟡`.
