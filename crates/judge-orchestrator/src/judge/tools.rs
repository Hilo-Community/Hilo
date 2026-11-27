//! The verification plan of a Node.js workspace: its own Vitest, Biome and TypeScript, run by
//! the Node.js found on `PATH`. Only for a **trusted** workspace: these are workspace-local
//! programs (`AGENTS.md` rule 6).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use host_core::{GatedCapability, WorkspaceTrustGate};
use sandbox_runner::ResourceLimits;

use super::pipeline::{ToolCommand, VerificationPlan};

/// Workspace-relative entry points, resolved by Node.js from the worktree it runs in.
const VITEST: &str = "node_modules/vitest/vitest.mjs";
const BIOME: &str = "node_modules/@biomejs/biome/bin/biome";
const TSC: &str = "node_modules/typescript/lib/tsc.js";

/// Node.js and what the sandbox must let it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeToolchain {
    pub node: PathBuf,
    pub read_only_paths: Vec<PathBuf>,
}

impl NodeToolchain {
    /// `node` from an absolute `PATH` entry, with its installation directory shared read-only.
    #[must_use]
    pub fn from_path() -> Option<Self> {
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        let found = std::env::split_paths(&std::env::var_os("PATH")?)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())?;
        Self::at(&found)
    }

    #[must_use]
    pub fn at(node: &Path) -> Option<Self> {
        let node = node.canonicalize().ok()?;
        let bin = node.parent()?.to_path_buf();
        // `<prefix>/bin/node`: the prefix holds Node's own libraries; never share `/`.
        let share = match bin.parent() {
            Some(prefix) if prefix.parent().is_some() => prefix.to_path_buf(),
            _ => bin,
        };
        Some(Self {
            node,
            read_only_paths: vec![share],
        })
    }
}

/// The plan for `gate`'s workspace, or `None` while it is untrusted. A tool the workspace
/// does not install is left out of the plan.
#[must_use]
pub fn node_verification_plan(
    gate: &WorkspaceTrustGate,
    toolchain: &NodeToolchain,
    limits: ResourceLimits,
) -> Option<VerificationPlan> {
    gate.authorize(GatedCapability::WorkspaceLocalBinary).ok()?;
    let root = gate.workspace_root();
    let tool = |entry: &str| {
        root.join(entry).is_file().then(|| ToolCommand {
            program: toolchain.node.clone(),
            leading_args: vec![OsString::from(entry)],
        })
    };
    Some(VerificationPlan {
        vitest: tool(VITEST),
        biome: tool(BIOME),
        typescript: tool(TSC),
        full_suite: true,
        read_only_paths: toolchain.read_only_paths.clone(),
        limits,
    })
}
