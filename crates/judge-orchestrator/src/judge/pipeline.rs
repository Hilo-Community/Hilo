//! `JudgePipeline` (`LOT-015`): one verification of one snapshot.
//!
//! 1. An invisible Git checkpoint records the workspace as the user sees it (`LOT-011`).
//! 2. A Copy-on-Write task worktree is made from it; the snapshot under test (the compiled
//!    `ChangeSet` result) is written there, never in the user's workspace.
//! 3. In the native OS sandbox (`LOT-012`, fail closed, no network): the impacted tests first,
//!    for the first `🟢/🔴` verdict, then Biome, the TypeScript checker and the whole suite.
//! 4. The result is an `Evidence` bound to the exact content hashes tested, signed by the Host.
//!
//! The commands are the Host's, from the plan: no model output chooses a program or an
//! argument. What they execute (tests importing the agent's code) runs inside the sandbox.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use host_core::{
    ropey::Rope, Checkpoint, CheckpointError, CheckpointKind, CheckpointManager, DiagnosticEntry,
    DiagnosticSeverity, DiagnosticSource, Evidence, EvidenceArtifact, ExecutedCommandRecord,
    HiloValidationState, PreviewRuntimeReport, QualityDiagnosticsSummary, TaskWorktree,
    UnsavedBuffer, CONTRACT_SCHEMA_VERSION, TESTED_FILE_ARTIFACT_KIND,
};
use sandbox_runner::{ResourceLimits, RunOutput, SandboxError, SandboxRunner, SandboxSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::compiler::{hash, result_digest, CompiledChangeSet};
use super::diagnostics::{parse_biome, parse_tsc, parse_vitest, vitest_counts, MAX_ITEMS_PER_RUN};
use crate::policy::{AuthorityError, HostAuthority, SignedEvidence};

/// A Node.js tool: the absolute program and the arguments before the tool's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCommand {
    pub program: PathBuf,
    pub leading_args: Vec<OsString>,
}

/// Which checks run and with what. Built by the Host from the trusted workspace.
#[derive(Debug, Clone)]
pub struct VerificationPlan {
    /// `vitest`: impacted tests, then the whole suite.
    pub vitest: Option<ToolCommand>,
    /// `biome check` on the changed files.
    pub biome: Option<ToolCommand>,
    /// The TypeScript checker (`tsc --noEmit`), when the workspace has a `tsconfig.json`.
    pub typescript: Option<ToolCommand>,
    /// Run the whole test suite after everything else passed.
    pub full_suite: bool,
    /// Toolchain directories the commands may read, never write.
    pub read_only_paths: Vec<PathBuf>,
    pub limits: ResourceLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepKind {
    ImpactedTests,
    Lint,
    TypeCheck,
    FullSuite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReport {
    pub kind: StepKind,
    pub record: ExecutedCommandRecord,
    pub passed: bool,
    pub items: Vec<DiagnosticEntry>,
}

/// Reported while a verification runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeProgress {
    /// The first `🟢/🔴` verdict, on the impacted tests (or the first check that ran).
    FirstVerdict {
        state: HiloValidationState,
        elapsed: Duration,
    },
    StepFinished {
        kind: StepKind,
        passed: bool,
    },
}

/// The snapshot to verify: every file that differs from the workspace, with its content.
#[derive(Debug, Clone)]
pub struct Verification<'a> {
    /// The checkpoint task id: `A-Z a-z 0-9 . _ -`, unique per verification.
    pub checkpoint_task: &'a str,
    pub task_id: &'a str,
    /// Workspace-relative path → content under test.
    pub files: &'a BTreeMap<String, String>,
    /// The user's unsaved buffers (workspace-relative path → text): what the checkpoint and the
    /// worktree show for files outside the snapshot.
    pub buffers: &'a BTreeMap<String, String>,
    /// Workspace-relative test files covering the change (Test Impact Analysis, `LOT-007`).
    pub impacted_tests: &'a [String],
}

#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub evidence: SignedEvidence,
    pub steps: Vec<StepReport>,
    pub first_verdict_after: Option<Duration>,
    /// Same failure, same signature: what the correction loop compares between attempts.
    pub failure_signature: Option<String>,
    pub before: Checkpoint,
}

impl JudgeOutcome {
    #[must_use]
    pub fn verdict(&self) -> HiloValidationState {
        self.evidence.evidence.verdict
    }
}

#[derive(Debug, Error)]
pub enum JudgeError {
    #[error("checkpoint: {0}")]
    Checkpoint(#[from] CheckpointError),
    /// The sandbox could not run a check. Fail closed: no verdict, and nothing ran on the host.
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("invalid snapshot path {0:?}")]
    InvalidPath(String),
    #[error("evidence signature: {0}")]
    Authority(#[from] AuthorityError),
    #[error("worktree I/O on {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct JudgePipeline {
    checkpoints: Arc<CheckpointManager>,
    runner: Arc<dyn SandboxRunner>,
    authority: Arc<HostAuthority>,
    plan: VerificationPlan,
}

impl JudgePipeline {
    pub fn new(
        checkpoints: Arc<CheckpointManager>,
        runner: Arc<dyn SandboxRunner>,
        authority: Arc<HostAuthority>,
        plan: VerificationPlan,
    ) -> Self {
        Self {
            checkpoints,
            runner,
            authority,
            plan,
        }
    }

    #[must_use]
    pub fn authority(&self) -> &HostAuthority {
        &self.authority
    }

    #[must_use]
    pub fn workspace_root(&self) -> PathBuf {
        self.checkpoints.workspace_root()
    }

    /// Verifies the result of `compiled` (`JudgePipeline::evaluate_changeset` of the spec).
    pub fn evaluate_changeset(
        &self,
        compiled: &CompiledChangeSet,
        attempt: u32,
        buffers: &BTreeMap<String, String>,
        impacted_tests: &[String],
        on_progress: &mut dyn FnMut(JudgeProgress),
    ) -> Result<JudgeOutcome, JudgeError> {
        let files: BTreeMap<String, String> = compiled
            .files
            .iter()
            .map(|file| (file.file_path.clone(), file.result_text.clone()))
            .collect();
        let checkpoint_task = format!("{}.{attempt}", compiled.task_id);
        self.evaluate(
            &Verification {
                checkpoint_task: &checkpoint_task,
                task_id: &compiled.task_id,
                files: &files,
                buffers,
                impacted_tests,
            },
            on_progress,
        )
    }

    pub fn evaluate(
        &self,
        request: &Verification<'_>,
        on_progress: &mut dyn FnMut(JudgeProgress),
    ) -> Result<JudgeOutcome, JudgeError> {
        for path in request.files.keys().chain(request.buffers.keys()) {
            if !is_plain_relative(path) {
                return Err(JudgeError::InvalidPath(path.clone()));
            }
        }
        let started = Instant::now();
        let root = self.checkpoints.workspace_root();
        let before_buffers: Vec<(PathBuf, &str)> = request
            .buffers
            .iter()
            .map(|(path, text)| (root.join(path), text.as_str()))
            .collect();
        let before = self.checkpoints.create_checkpoint(
            request.checkpoint_task,
            CheckpointKind::BeforeAgent,
            &unsaved(&before_buffers),
        )?;
        let worktree = self
            .checkpoints
            .provision_worktree_or_copy(request.checkpoint_task)?;
        let result = self.run_checks(request, &worktree, started, on_progress);
        let released = self.checkpoints.release_worktree(request.checkpoint_task);
        let steps = result?;
        released?;

        // The agent's result, recorded for rollback (`LOT-011`): the buffers with the snapshot.
        let mut after = request.buffers.clone();
        after.extend(request.files.iter().map(|(p, t)| (p.clone(), t.clone())));
        let after_buffers: Vec<(PathBuf, &str)> = after
            .iter()
            .map(|(path, text)| (root.join(path), text.as_str()))
            .collect();
        self.checkpoints.create_checkpoint(
            request.checkpoint_task,
            CheckpointKind::AfterAgent,
            &unsaved(&after_buffers),
        )?;

        let first_verdict_after = steps.first_verdict;
        let evidence = self.evidence(request, &before, &steps.reports);
        let failure_signature = failure_signature(&steps.reports);
        Ok(JudgeOutcome {
            evidence: self.authority.sign_evidence(evidence)?,
            steps: steps.reports,
            first_verdict_after,
            failure_signature,
            before,
        })
    }

    fn run_checks(
        &self,
        request: &Verification<'_>,
        worktree: &TaskWorktree,
        started: Instant,
        on_progress: &mut dyn FnMut(JudgeProgress),
    ) -> Result<Steps, JudgeError> {
        for (path, text) in request.files {
            let target = worktree.workspace.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|source| JudgeError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            std::fs::write(&target, text).map_err(|source| JudgeError::Io {
                path: target.clone(),
                source,
            })?;
        }
        let mut read_only = self.plan.read_only_paths.clone();
        for store in &worktree.shared_package_stores {
            if let Ok(real) = store.source.canonicalize() {
                read_only.push(real);
            }
        }
        read_only.sort();
        read_only.dedup();
        let context = StepContext {
            runner: self.runner.as_ref(),
            worktree,
            read_only: &read_only,
            limits: self.plan.limits,
            files: request.files,
        };

        let mut steps = Steps::default();
        let mut report = |steps: &mut Steps, report: StepReport| {
            on_progress(JudgeProgress::StepFinished {
                kind: report.kind,
                passed: report.passed,
            });
            if steps.first_verdict.is_none() {
                let elapsed = started.elapsed();
                steps.first_verdict = Some(elapsed);
                on_progress(JudgeProgress::FirstVerdict {
                    state: if report.passed {
                        HiloValidationState::ValidatedGreen
                    } else {
                        HiloValidationState::FailedRed
                    },
                    elapsed,
                });
            }
            steps.reports.push(report);
        };

        if let Some(vitest) = &self.plan.vitest {
            let tests: Vec<&String> = request
                .impacted_tests
                .iter()
                .filter(|test| is_plain_relative(test))
                .collect();
            if !tests.is_empty() {
                let mut args = vitest_args(vitest);
                args.extend(tests.iter().map(|test| OsString::from(test.as_str())));
                let step = context.vitest_step(StepKind::ImpactedTests, vitest, args)?;
                report(&mut steps, step);
            }
        }
        if let Some(biome) = &self.plan.biome {
            let lintable: Vec<&String> = request
                .files
                .keys()
                .filter(|path| is_lintable(path))
                .collect();
            if !lintable.is_empty() {
                let mut args = biome.leading_args.clone();
                args.extend(["check", "--reporter=json", "--colors=off"].map(OsString::from));
                args.extend(lintable.iter().map(|path| OsString::from(path.as_str())));
                let step = context.biome_step(biome, args)?;
                report(&mut steps, step);
            }
        }
        if let Some(typescript) = &self.plan.typescript {
            if worktree.workspace.join("tsconfig.json").is_file() {
                let mut args = typescript.leading_args.clone();
                args.extend(["--noEmit", "-p", ".", "--pretty", "false"].map(OsString::from));
                let step = context.typescript_step(typescript, args)?;
                report(&mut steps, step);
            }
        }
        if let Some(vitest) = &self.plan.vitest {
            if self.plan.full_suite && steps.reports.iter().all(|step| step.passed) {
                let step = context.vitest_step(StepKind::FullSuite, vitest, vitest_args(vitest))?;
                report(&mut steps, step);
            }
        }
        Ok(steps)
    }

    fn evidence(
        &self,
        request: &Verification<'_>,
        before: &Checkpoint,
        steps: &[StepReport],
    ) -> Evidence {
        let file_hashes: Vec<(String, String)> = request
            .files
            .iter()
            .map(|(path, text)| (path.clone(), hash(text)))
            .collect();
        let tested_snapshot_hash = match file_hashes.as_slice() {
            [(_, only)] => only.clone(),
            many => result_digest(many.iter().map(|(p, h)| (p.as_str(), h.as_str()))),
        };
        let mut diagnostics = QualityDiagnosticsSummary::default();
        for item in steps.iter().flat_map(|step| &step.items) {
            let error = item.severity == DiagnosticSeverity::Error;
            match item.source {
                DiagnosticSource::Compiler | DiagnosticSource::Vtsls => {
                    if error {
                        diagnostics.vtsls_error_count += 1;
                    } else {
                        diagnostics.vtsls_warning_count += 1;
                    }
                }
                DiagnosticSource::Biome => {
                    if error {
                        diagnostics.biome_error_count += 1;
                    } else {
                        diagnostics.biome_warning_count += 1;
                    }
                }
                _ => {}
            }
            if diagnostics.items.len() < MAX_ITEMS_PER_RUN * 4 {
                diagnostics.items.push(item.clone());
            }
        }
        let verdict = if steps.is_empty() {
            // Nothing could check it: a human decides.
            HiloValidationState::ReviewYellow
        } else if steps.iter().all(|step| step.passed)
            && diagnostics.vtsls_error_count == 0
            && diagnostics.biome_error_count == 0
        {
            HiloValidationState::ValidatedGreen
        } else {
            HiloValidationState::FailedRed
        };
        let artifacts = file_hashes
            .iter()
            .map(|(path, digest)| EvidenceArtifact {
                name: path.clone(),
                kind: TESTED_FILE_ARTIFACT_KIND.into(),
                digest_blake3: digest.clone(),
                byte_size: request.files[path].len() as u32,
            })
            .collect();
        let evidence_id = blake3::hash(
            format!(
                "{}\0{}\0{}\0{tested_snapshot_hash}",
                request.task_id, request.checkpoint_task, before.commit
            )
            .as_bytes(),
        )
        .to_hex()[..32]
            .to_string();
        Evidence {
            schema_version: CONTRACT_SCHEMA_VERSION,
            evidence_id,
            task_id: request.task_id.to_string(),
            tested_snapshot_hash,
            checkpoint_ref: before.reference.clone(),
            commands: steps.iter().map(|step| step.record.clone()).collect(),
            diagnostics,
            // The Preview Service joins the evidence in LOT-016: no runtime check ran here.
            preview_report: PreviewRuntimeReport {
                inspected_url: String::new(),
                console_error_count: 0,
                uncaught_exception_count: 0,
                failed_network_request_count: 0,
                crashed: false,
                dom_snapshot_hash: None,
            },
            artifacts,
            verdict,
        }
    }
}

#[derive(Default)]
struct Steps {
    reports: Vec<StepReport>,
    first_verdict: Option<Duration>,
}

struct StepContext<'a> {
    runner: &'a dyn SandboxRunner,
    worktree: &'a TaskWorktree,
    read_only: &'a [PathBuf],
    limits: ResourceLimits,
    files: &'a BTreeMap<String, String>,
}

impl StepContext<'_> {
    fn run(
        &self,
        tool: &ToolCommand,
        args: Vec<OsString>,
    ) -> Result<(RunOutput, ExecutedCommandRecord), JudgeError> {
        let mut spec = SandboxSpec::new(&self.worktree.path, &tool.program);
        spec.cwd = self.worktree.workspace.clone();
        spec.args = args;
        spec.read_only_paths = self.read_only.to_vec();
        spec.limits = self.limits;
        spec.env = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("CI".into(), "1".into()),
            ("NO_COLOR".into(), "1".into()),
            ("FORCE_COLOR".into(), "0".into()),
        ]);
        let started = Instant::now();
        let output = self.runner.run(&spec)?;
        let exit_code = match output.termination {
            sandbox_runner::Termination::Exited {
                code: Some(code), ..
            } => code,
            _ => -1,
        };
        let record = ExecutedCommandRecord {
            command: tool.program.to_string_lossy().into_owned(),
            args: spec
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            exit_code: if output.incomplete && exit_code == 0 {
                -1
            } else {
                exit_code
            },
            duration_ms: started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
            impacted_tests_only: false,
        };
        Ok((output, record))
    }

    fn text_of(&self, path: &str) -> Option<Rope> {
        if let Some(text) = self.files.get(path) {
            return Some(Rope::from_str(text));
        }
        if !is_plain_relative(path) {
            return None;
        }
        std::fs::read_to_string(self.worktree.workspace.join(path))
            .ok()
            .map(|text| Rope::from_str(&text))
    }

    fn vitest_step(
        &self,
        kind: StepKind,
        tool: &ToolCommand,
        args: Vec<OsString>,
    ) -> Result<StepReport, JudgeError> {
        let (output, mut record) = self.run(tool, args)?;
        record.impacted_tests_only = kind == StepKind::ImpactedTests;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut items = parse_vitest(&stdout, &self.worktree.workspace);
        let counts = vitest_counts(&stdout);
        let passed = record.exit_code == 0 && counts.is_some_and(|(_, failed)| failed == 0);
        if !passed && items.is_empty() {
            items.push(tool_failure(DiagnosticSource::TestRunner, &output));
        }
        self.clean(&mut items);
        Ok(StepReport {
            kind,
            record,
            passed,
            items,
        })
    }

    fn biome_step(
        &self,
        tool: &ToolCommand,
        args: Vec<OsString>,
    ) -> Result<StepReport, JudgeError> {
        let (output, record) = self.run(tool, args)?;
        let mut items = parse_biome(&String::from_utf8_lossy(&output.stdout), &|path| {
            self.text_of(path)
        });
        let errors = items
            .iter()
            .any(|item| item.severity == DiagnosticSeverity::Error);
        let passed = record.exit_code == 0 && !errors;
        if !passed && items.is_empty() {
            items.push(tool_failure(DiagnosticSource::Biome, &output));
        }
        self.clean(&mut items);
        Ok(StepReport {
            kind: StepKind::Lint,
            record,
            passed,
            items,
        })
    }

    fn typescript_step(
        &self,
        tool: &ToolCommand,
        args: Vec<OsString>,
    ) -> Result<StepReport, JudgeError> {
        let (output, record) = self.run(tool, args)?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mut items = parse_tsc(&text, &|path| self.text_of(path));
        let passed = record.exit_code == 0;
        if !passed && items.is_empty() {
            items.push(tool_failure(DiagnosticSource::Compiler, &output));
        }
        self.clean(&mut items);
        Ok(StepReport {
            kind: StepKind::TypeCheck,
            record,
            passed,
            items,
        })
    }

    /// Messages must not depend on where the worktree was: the same failure in two attempts
    /// has to read (and hash) the same.
    fn clean(&self, items: &mut [DiagnosticEntry]) {
        let roots = [
            self.worktree.workspace.to_string_lossy().into_owned(),
            self.worktree
                .workspace
                .canonicalize()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ];
        for item in items {
            for root in roots.iter().filter(|root| !root.is_empty()) {
                item.message = item.message.replace(root.as_str(), ".");
                item.file_path = item.file_path.replace(root.as_str(), ".");
            }
            item.message = item
                .message
                .lines()
                .filter(|line| !line.trim_start().starts_with("at "))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
}

fn vitest_args(tool: &ToolCommand) -> Vec<OsString> {
    let mut args = tool.leading_args.clone();
    // Worker threads, not forks: the sandbox forbids signalling other processes, so a forks
    // pool cannot stop its workers and waits for its own timeout.
    args.extend(
        [
            "run",
            "--pool=threads",
            "--reporter=json",
            "--passWithNoTests",
        ]
        .map(OsString::from),
    );
    args
}

fn tool_failure(source: DiagnosticSource, output: &RunOutput) -> DiagnosticEntry {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: String = stderr
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    DiagnosticEntry {
        file_path: String::new(),
        span_utf16: Default::default(),
        line_col: Default::default(),
        source,
        severity: DiagnosticSeverity::Error,
        code: format!("{:?}", output.termination),
        message: tail,
    }
}

fn unsaved<'a>(buffers: &'a [(PathBuf, &'a str)]) -> Vec<UnsavedBuffer<'a>> {
    buffers
        .iter()
        .map(|(path, text)| UnsavedBuffer { path, text })
        .collect()
}

/// A path the snapshot may name: relative, no `..`, no root or prefix.
pub(crate) fn is_plain_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_lintable(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc", ".css",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
}

/// What failed, independent of timing and location: failing steps and their diagnostics.
fn failure_signature(steps: &[StepReport]) -> Option<String> {
    let mut parts: Vec<String> = steps
        .iter()
        .filter(|step| !step.passed)
        .flat_map(|step| {
            let kind = format!("{:?}", step.kind);
            let mut lines = vec![kind.clone()];
            lines.extend(step.items.iter().map(|item| {
                format!(
                    "{kind}\0{:?}\0{}\0{}\0{}",
                    item.source, item.file_path, item.code, item.message
                )
            }));
            lines
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    Some(
        blake3::hash(parts.join("\n").as_bytes())
            .to_hex()
            .to_string(),
    )
}
