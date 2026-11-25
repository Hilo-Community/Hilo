# host-core dependency reviews

## `portable-pty = 0.9.0` (terminal, LOT-010) — reviewed 2026-11-13

Pinned exactly. Part of the WezTerm project (maintainer: Wez Furlong). Reviewed from the
published crate in the local cargo registry:

- `src/unix.rs` (SHA-256 `14c85213b32074d71c8af334531fd027d6a0558dee830282620c88bab056c4b3`):
  `openpty`, then `std::process::Command` with a `pre_exec` that resets signal dispositions
  and the signal mask, calls `setsid()` (the shell leads its own session and process group,
  which is what our orphan watchdog targets), sets the controlling tty and closes inherited
  descriptors. Its `unsafe` is libc FFI for these calls, `ttyname_r`, `ioctl(TIOCSWINSZ)` and
  `fcntl`. No network, no file writes, no build script.
- `src/cmdbuilder.rs` (SHA-256 `db95387276f7f5a2e6bfd27a0c21ad99090f5d29e3663ad0446becec86748283`):
  builds the command; with `new_default_prog` it runs the user's login shell (`getpwuid`
  `pw_shell`, or `$SHELL`) as `-<shell>`. The environment is cleared, then the base
  environment captured at construction is set again, plus `SHELL` and what we add (`TERM`,
  `COLORTERM`, `TERM_PROGRAM`). The working directory falls back to `$HOME` when the requested
  one is not a directory.
- Published package checksum in `Cargo.lock`:
  `b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e`.
- New crates in the lockfile: `nix 0.28`, `filedescriptor`, `serial2`, `shell-words`,
  `downcast-rs`, `anyhow` (plus Windows-only `winapi`, `winreg`, `shared_library`, `lazy_static`,
  `bitflags 1`). Not reviewed line by line; `nix` and `anyhow` are widely used.

How we use it: only from `terminal.rs`, after `WorkspaceTrustGate::authorize(Terminal)`; the
shell's group is killed when the session is dropped and by an `OrphanWatchdog` if the Host
dies abruptly. Not behind a cargo feature: the terminal is part of the core editor (LOT-010).
Any version change needs another review.

## `git2 = 0.21.0` + `libgit2-sys = 0.18.8+1.9.7` (checkpoints, LOT-011) — reviewed 2026-11-14

Pinned exactly (`git2`). The rust-lang project's bindings to libgit2 1.9.7 (published
2026-05-18). Built with `default-features = false` and `vendored-libgit2`: **no** `https`/`ssh`
features, so no OpenSSL, libssh2 or network transport is compiled in, and every platform links
the same libgit2 built from the crate's bundled sources.

- `libgit2-sys/build.rs` (SHA-256 `0428eccc1a527dba5b208d84e622935821bdf89a4a396ba706857cb3298acd45`):
  compiles the bundled C sources with `cc`. It runs `git submodule update` only when those
  sources are missing, which is never the case for the published crate. No download.
- Published package checksums in `Cargo.lock`: `git2`
  `ddddbf932745a6be37109b6112d3ee09696106f848449069d3a57bba937ab82e`, `libgit2-sys`
  `7f7c568b25d7489bc3fb2988ed69ab111d2944d2f5fec3d5c987fe545ea97b50`.
- New crates in the lockfile: `libgit2-sys`, `libz-sys` (zlib, also vendored when missing),
  `log`. The C sources of libgit2 were not reviewed line by line.

How we use it: only from `src/checkpoint/`. libgit2 runs no hook, no `.gitattributes` filter
driver and no other program (checked by `checkpoints_run_no_git_hook`), which is why checkpoints
are not behind `WorkspaceTrustGate`. Blobs are created with `git_blob_create_from_disk` /
`from_buffer`, which apply no filter: restores are byte-exact. It never writes `.git/index`,
`HEAD` or a branch; it writes objects, `refs/hilo/checkpoints/*` and
`<git dir>/hilo_tmp_index`. Any version change needs another review.

## `reflink-copy = 0.1.30` (Copy-on-Write task worktrees, LOT-011) — reviewed 2026-11-14

Pinned exactly; published 2026-06-18 (0.1.31 to 0.1.33 were less than two weeks old). From the
cargo-bins project. Reviewed from the published crate in the local cargo registry:

- `src/sys/unix/macos.rs` (SHA-256 `db099e9addb9fe40a9e4bd4f75c6dab05521865b302848dd4ddb2e4fe0b48293`):
  one `clonefile(src, dst, CLONE_NOOWNERCOPY)` FFI call. Without `CLONE_NOFOLLOW` it follows a
  symbolic link at `src`: we only clone sources whose `lstat` is a regular file (or a directory
  checked entry by entry) matching the checkpoint's stat cache.
- `src/sys/unix/linux.rs` (SHA-256 `badfe8a09a4940a65715154f3e04ea3910dcdc1419040b219762a86e3008a708`):
  `ioctl_ficlone` through `rustix` into a new file created with `O_EXCL`, removed on failure.
- `src/sys/windows_impl.rs` (SHA-256 `07e054084c036645fcccd5f40266ab55b9080a4301f12d01313d860238582db2`):
  `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS / Dev Drive). Its own documentation calls it
  untested; on NTFS it fails, which we report as `CowUnavailable`.
- `src/lib.rs` (SHA-256 `f6236077c1f158ae4dbdee748797a877c3eab397a06bedd2f22bda517a56ecf2`):
  `reflink` never falls back to copying (only `reflink_or_copy` does, which we do not call).
- No build script, no network. Published package checksum in `Cargo.lock`:
  `d9dd7ab4af0363d5ccfd2838d782a28196cf32a5cc2e4fe3c5dc83f2be588b8b`.
- New crates in the lockfile: `cfg-if`; Windows-only `windows` 0.62 bindings.

How we use it: only `reflink(source, target)` from `src/checkpoint/worktree.rs`. Any error is
fatal (`CowUnavailable`), and the partial worktree is removed: no silent full copy.
Any version change needs another review.

## `rustix = 1.1.6` (`process` feature, Unix only)

Already in the dependency tree (via `tempfile`). Used for one call: `kill_process_group`
(safe API), so `host-core` keeps `unsafe_code = "forbid"`.

## `sandbox-windows` (workspace crate, Windows only)

Workspace crate that isolates Win32 FFI behind a safe API. Used on Windows for `SupervisedJob`
(a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), so `host-core` keeps
`unsafe_code = "forbid"` while supervising child process trees and terminals.

## `@vtsls/language-server = 0.3.0`, `@xterm/xterm = 6.0.0`, `@xterm/addon-fit = 0.11.0` (npm)

Pinned exactly. `vtsls` is a dev dependency so tests run the real server (it bundles the
TypeScript language service; it is not shipped with the app by this LOT). xterm.js 6.0.0 and
its fit addon are the upstream terminal emulator used by VS Code; published 2025-12-22.

## vscode-js-debug `v1.140.0`, DAP build (debugger, LOT-010) — 2026-11-13

Not on npm; fetched by `pnpm fetch:js-debug` (`scripts/fetch-js-debug.mjs`) from the GitHub
release of `microsoft/vscode-js-debug` (published 2026-09-23). The archive is verified against a
pinned SHA-256 (`27dab92937ec1ab35821ae955aac867544fe06a1b6307229049f2d789af10968`, the digest
GitHub publishes for the asset) before it is extracted into the git-ignored `tools/` folder.
The app never downloads it; it looks for it at `$HILO_JS_DEBUG`, in its data folder, or (in
development builds) in `tools/`.

Review: provenance only (publisher, release, digest). The 2.6 MB bundled, minified JavaScript
was **not** reviewed line by line. What limits it on our side: it starts only for a trusted
workspace (`GatedCapability::DebugAdapter`), as a supervised process group (the debuggee is in
that group and dies with it, including on an abrupt Host exit); we answer none of its reverse
requests except `startDebugging`, whose configuration we rebuild from its target id alone;
`runInTerminal` is refused.

Known limit: its DAP server listens on a TCP port of `127.0.0.1` without authentication for the
length of a session. Another process of the same user could connect to it and launch programs;
such a process can already run programs as that user, so this does not cross a privilege
boundary, but it is wider than a stdio pipe.
