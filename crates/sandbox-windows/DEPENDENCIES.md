# sandbox-windows dependency review

This is the only crate of the workspace with `unsafe` code (decided 2026-11-15). Every other crate
keeps `unsafe_code = "forbid"`. Its lint table (`Cargo.toml`) allows `unsafe` but denies an
`unsafe` block without a `// SAFETY:` comment and an `unsafe fn` without a `# Safety` section.

## `windows-sys = 0.61.2` — reviewed 2026-11-15

Pinned exactly. Microsoft's raw bindings (`github.com/microsoft/windows-rs`, MIT OR Apache-2.0),
generated from Windows metadata: declarations of the Win32 functions, types and constants, no
logic. `build = false` (no build script). Already in the lockfile through other crates (Tokio,
`rustix`, `git2` dependencies), so it adds no new crate; only feature flags are added here.
Published package checksum in `Cargo.lock`:
`ae137229bcbd6cdf0f7b80a31df61766145077ddf49416a728b02cb3921ff3fc` (the `.crate` archive).

Features: `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`,
`Win32_Security_Isolation`, `Win32_Storage_FileSystem`, `Win32_System_JobObjects`,
`Win32_System_Pipes`, `Win32_System_Threading`.

## The Win32 functions the crate calls (approved list)

| Purpose | Functions |
|---|---|
| AppContainer profile | `CreateAppContainerProfile`, `DeleteAppContainerProfile`, `DeriveAppContainerSidFromAppContainerName` (cleanup after a dead Host), `FreeSid` |
| Access to the worktree (ACL) | `GetNamedSecurityInfoW`, `SetEntriesInAclW`, `SetNamedSecurityInfoW`, `LocalFree` |
| Refusing links before granting | `GetFileInformationByHandle` (number of hard links of a file) |
| Starting the process | `InitializeProcThreadAttributeList`, `UpdateProcThreadAttribute`, `DeleteProcThreadAttributeList`, `CreateProcessW`, `ResumeThread` |
| Pipes | `CreatePipe`, `SetHandleInformation` |
| Job Object | `CreateJobObjectW`, `SetInformationJobObject`, `AssignProcessToJobObject`, `TerminateJobObject` |
| Waiting and killing | `WaitForSingleObject`, `GetExitCodeProcess`, `TerminateProcess` |

22 functions. Handles are owned by `std::os::windows::io::OwnedHandle` (closed by the standard
library), errors come from `std::io::Error::last_os_error`, so `CloseHandle` and `GetLastError`
are not called directly. No capability is ever requested for the AppContainer: no network, no
access to the user's files. The command line and environment block are built by the safe
`command_line` module (unit-tested on every OS).
