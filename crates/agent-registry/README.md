# agent-registry

The Host side of the Settings screen's **ACP Registry**: it reads the public catalog of
[Agent Client Protocol](https://agentclientprotocol.com) agents, installs and removes them.
Running an installed agent is not done here: it goes through `sandbox-runner` like any other
process (`judge-orchestrator::acp`).

Ported in spirit from the registry of an earlier prototype (React + Python); this version is
Rust and fail-closed, so some behavior is deliberately stricter.

## What is installed, and how

| Distribution | Needs | Verified by |
| --- | --- | --- |
| `binary` (`.zip`, `.tar.gz`) | nothing | SHA-256 published in the catalog |
| `npm` | Node.js >= 22 and npm of this computer | exact version, `--ignore-scripts` |
| `uv` | | not supported |

When an entry offers several, a verified binary wins over npm. Each agent has one installation;
installing another version replaces it. Files live under `<app data>/agent-registry/`:
`installations.json`, `registry-cache.json` and `packages/<agent>/<version>/`.

## Why this is safe on the Host

Installing needs the network and writes the agent's own folder, so it cannot run in the sandbox.
What contains it instead:

- The catalog is read over HTTPS from one address, **without redirects**; a body that is not a
  catalog is not saved. Offline, the saved catalog is shown and the screen says so.
- A binary is installed only if the catalog publishes its SHA-256 **and** the download matches.
  Entries without one are shown as blocked, not installed unverified (Cursor today).
- An npm package must be pinned to an exact version, is fetched from `registry.npmjs.org`
  whatever the user's npm configuration says, and is installed with `--ignore-scripts`: the code
  is downloaded, none of it runs. Afterwards the installed name and version must match, and the
  executable must be a real file inside the install (links are resolved).
- Archives are unpacked by our own loop, never `unpack`/`extract`: a name that is absolute or
  contains `..`, a link or special file, a repeated name, a file over 1 GiB, more than 2 GiB in
  total or more than 20,000 entries stops the install. Permission bits are masked: no set-id.
- An install is prepared in `packages/.staging/` and moved into place only when complete; a
  failure or crash leaves nothing (the folder is cleaned at the next start).
- A folder is removed only if it is inside `packages/` (checked after resolving links; a link
  itself is refused). `installations.json` is data and is not trusted to point anywhere.
- The Tauri command asks the user in a **native dialog** before anything is fetched, naming the
  exact version, and installs exactly that version: if the catalog changed meanwhile it refuses.
- Agents listed in `HIDDEN_AGENT_IDS` cannot be installed even by name. `env` published by the
  registry is ignored: a launched agent gets an empty environment (see the ACP README).

## Dependencies

`sha2`, `flate2` (reused from the lockfile), `ureq` 2.12.1 with rustls and bundled roots only (no
cookies, no proxies from the environment; already in the lockfile through `hf-hub`), and two new
read-only crates, `tar` 0.4.46 and `zip` 4.2.0 (the latest compatible with Rust 1.80; deflate
only). All pinned exactly. `ring` (through rustls) needs a C compiler for the target, so this
crate cannot be cross-checked from macOS, like `host-core`; Linux and Windows are covered by CI.

## Not done

- **Node.js is the computer's.** Coloride does not download one. It looks in `PATH`, then the
  places a GUI app does not see (Homebrew, nvm, Volta, Windows install folders);
  `COLORIDE_NODE` forces a path. Volta's shims are not recognized (npm is not beside them).
- `.tar.bz2` and bare executables (about 8 catalog builds) and uv agents are listed as not
  supported.
- Links to a repository or website are not offered: opening a URL needs a Tauri opener plugin.
- Installed agents are not yet selectable in the agent panel (`AgentRegistry::launch_command`
  returns what the supervisor needs). Running Codex also needs provider egress and credentials,
  still undecided; Windows refuses read-only sandbox shares.
- No progress bar: an install shows "Installing…" until it finishes (npm may take minutes).

## Tests

`cargo test -p agent-registry`: unit tests per module and `tests/install.rs`, which drives the
whole registry with a fake network and a fake Node (npm cases need `/bin/sh`, so Unix only).
By hand, with the network: `cargo test -p agent-registry --test install -- --ignored --nocapture`
installs the real Claude (binary) and Codex (npm), starts nothing, and removes them again;
`-- --ignored` in `--lib` parses the real catalog.

Deliberate mutations, each made one of these tests fail and was restored (checked with `cmp`):
skip the checksum comparison; remove both checksum guards; accept `..` in a name; accept an
absolute name; accept links in a tar; let a repeated name overwrite; remove the removal
confinement; drop `--ignore-scripts`; skip the version check; follow redirects for the catalog;
install a hidden agent; skip the check that an npm executable stays inside its install; keep the
staging folder after a failure.
