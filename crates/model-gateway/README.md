# model-gateway (LOT-014)

The one door between Hilo and a model.

- **Registry** (`registry.rs`): versioned list of models. Each entry states its runtime kind and
  where its prompt goes (`AuthorizedDestination`). A local runtime must declare `LocalOnly` and
  an `http://` loopback **IP literal** endpoint (a host name could resolve off the machine); a
  remote one must use `https`. The built-in entries are Qwen2.5-Coder 1.5B and 7B base models on
  Ollama (base models support fill-in-the-middle).
- **Gateway** (`gateway.rs`): for one request, in this order: resolve the named entry, check the
  workspace `PrivacyPolicy` against its destination, find its backend, reserve the token
  budget, make exactly one backend call. **There is no fallback**: a failing runtime is an
  error, and no other entry is ever tried. A cancelled or failed call keeps its input estimate
  spent, so a flood of cancelled keystrokes still counts.
- **Ollama** (`ollama.rs`): `POST /api/generate` with `suffix` (Ollama applies the model's FIM
  template), `keep_alive` 30 min. The client ignores `HTTP(S)_PROXY`/`ALL_PROXY` and follows no
  redirect, so local traffic cannot be routed off the machine.
- **Window** (`window.rs`): 2000 characters before the cursor, 300 after, 32 output tokens. The
  start is anchored in steps of 512 characters, so it does not move while typing and Ollama
  reuses the prompt it already evaluated.
- **Memory** (`memory.rs`): picks the largest local model that fits in free memory, or says none
  does (then use a key of your own).

BYOK keys for agents stay in the OS keyring (`crates/agent-gateway/src/credentials.rs`); no key
is ever placed in a sandbox environment.

## Desktop wiring

`apps/desktop-tauri/src-tauri/src/commands/completion.rs`: the editor sends the document id,
session generation, revision and UTF-16 cursor; the Host reads the text from its own rope
(converted by host-core), waits briefly if it has not reached that revision yet, and answers
only for that exact revision. Every `apply_binary_transaction_command` (a keystroke) cancels the
call in flight. Completion uses `PrivacyPolicy::strict_local()` and only local backends.

## Tests

- `tests/lot014_gateway.rs`: provider outage with another cloud configured (0 connections to it),
  FIM request shape and usage, cancellation (`cancel()` < 1 ms, call ended < 50 ms), budget
  reserved before contact.
- `tests/ollama_ignores_proxy.rs`: proxy variables do not reroute the local runtime.
- Real latency (ignored by default, needs `ollama pull qwen2.5-coder:1.5b-base`). It sends what
  the editor sends while someone types in real code: 8 places, each a cold first request then 5
  keystrokes.

  ```bash
  cargo test -p model-gateway --test lot014_gateway -- --ignored --nocapture
  ```

### Measured (2026-11-27, Apple M1 Pro 16 GB, macOS 27.0.1, Ollama 0.40.18, Q4_K_M)

| Case | p50 | p95 | max |
|---|---|---|---|
| While typing (40 requests) | 161 ms | **463 ms** | 465 ms |
| First request at a new place (8) | 864 ms | — | 895 ms |

The editor waits 120 ms after the last keystroke before asking, on top of these. Without the
anchored window (the start moving with every keystroke) the p95 was 2.0 s: about 1 ms per prompt
token is spent evaluating a prompt the runtime has not seen, and 11 ms per generated token.

## Not here

A `llama.cpp` backend (the `LocalLlamaCpp` kind is accepted by the registry but no backend is
registered), a BYOK or managed-cloud completion backend (managed cloud is LOT-017).
