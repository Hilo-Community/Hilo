//! Launching a process inside a Windows AppContainer and a Job Object.
//!
//! This is the only crate of the workspace that contains `unsafe` code: the Win32 calls have no
//! safe binding. It is kept small and exposes a safe API. Every `unsafe` block carries a
//! `// SAFETY:` comment, which the lint table of `Cargo.toml` enforces. The complete list of
//! Win32 functions it calls is in `DEPENDENCIES.md`.
//!
//! * The **AppContainer** is the isolation: a low-integrity token that, with no capability
//!   granted, has no network (loopback included) and cannot read or write the user's files. The
//!   only places it may use are the directories in [`LaunchConfig::writable_dirs`], whose ACL
//!   gets an entry for this run only, removed afterwards. A directory containing a link to
//!   elsewhere (junction, symbolic or hard link) is refused, so that the change of permissions
//!   stays inside it. Each run is recorded in a journal, so that a run whose Host died is undone
//!   at the next start ([`recover_stale_runs`]).
//! * The **Job Object** is the supervision: the process tree cannot leave it, it is killed as a
//!   whole, and it carries the memory, process-count and CPU-time limits. The kernel kills the
//!   whole tree when the last handle to the job closes, even if the Host dies abruptly.
//!
//! On other systems the crate only offers the pure text helpers of [`command_line`].

pub mod command_line;

#[cfg(windows)]
mod acl;
#[cfg(windows)]
mod container;
#[cfg(windows)]
mod job;
#[cfg(windows)]
mod journal;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use job::SupervisedJob;
#[cfg(windows)]
pub use journal::recover_stale_runs;
#[cfg(windows)]
pub use process::{spawn, JobLimits, LaunchConfig, Process, Spawned, TreeKiller};
