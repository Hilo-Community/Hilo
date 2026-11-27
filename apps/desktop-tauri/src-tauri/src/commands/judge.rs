//! The Judge on the desktop (`LOT-015`).
//!
//! An ACP agent's file writes are proposals (`LOT-014`). When its turn ends, the Host turns the
//! proposals into a `ChangeSet`, compiles it against the live ropes and verifies the result in
//! the OS sandbox. A `🔴` result goes back to the agent (at most twice); the final result waits
//! for the user's review: Accept, Reject or Request Adjustment.
//!
//! Accepting never lets the webview write what it wants: the Host plans the exact edits, the
//! editor dispatches them as an agent transaction, and `authorize_transaction` refuses any
//! agent transaction that does not produce exactly the planned content.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use host_core::{
    ropey::Rope, CheckpointManager, DocumentService, EditorTransaction, HiloValidationState,
    TransactionOrigin, Utf16Span, WorkspaceTrustGate,
};
use judge_orchestrator::acp::ProposedEdit;
use judge_orchestrator::judge::*;
use judge_orchestrator::policy::HostAuthority;
use sandbox_runner::ResourceLimits;
use serde::Serialize;

use crate::bridge::DesktopBinaryEditorBridge;
use crate::state::DesktopAppState;

/// Event carrying a `JudgeEvent` to the UI.
pub(crate) const JUDGE_EVENT: &str = "hilo-judge";
/// Longest a verification step may run.
const STEP_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) type JudgeSink = Arc<dyn Fn(JudgeEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum JudgeEvent {
    Verifying {
        agent_id: u32,
        attempt: u32,
    },
    FirstVerdict {
        agent_id: u32,
        attempt: u32,
        state: HiloValidationState,
        elapsed_ms: u64,
    },
    /// A `🔴` result was sent back to the agent.
    Retrying {
        agent_id: u32,
        attempt: u32,
    },
    ReviewReady {
        review: ReviewSummary,
    },
    ReviewClosed {
        review_id: String,
        status: ReviewStatus,
    },
    Error {
        agent_id: u32,
        reason: String,
    },
    /// The validation ranges of a document changed (accepted change, re-verification).
    States {
        document_id: String,
        session_generation: u32,
        content_hash: String,
        decorations: Vec<StateDecoration>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum ReviewStatus {
    Pending,
    Accepted,
    Rejected,
    Adjusting,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewSummary {
    review_id: String,
    agent_id: u32,
    verdict: HiloValidationState,
    attempts: u32,
    status: ReviewStatus,
    /// Why the automatic corrections stopped, when they did.
    stopped: Option<String>,
    files: Vec<ReviewFile>,
    steps: Vec<StepSummary>,
    diagnostics: Vec<DiagnosticSummary>,
    /// What the changed components rendered (`LOT-016`): a static document for an iframe that
    /// runs no script.
    preview: Option<PreviewSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StepSummary {
    kind: StepKind,
    passed: bool,
    exit_code: i32,
    duration_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticSummary {
    file_path: String,
    line: u32,
    column: u32,
    source: host_core::DiagnosticSource,
    code: String,
    message: String,
}

/// A validation range of a document, as the editor draws it (1-based lines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StateDecoration {
    from_line: u32,
    to_line: u32,
    state: HiloValidationState,
    snapshot_hash: String,
}

/// What the editor must dispatch to accept a file, or the conflict to show instead.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum AcceptResponse {
    Apply {
        task_id: String,
        base_revision: u32,
        /// UTF-16 offsets of the current text, ascending (CodeMirror's `changes`).
        changes: Vec<EditorChange>,
        state: HiloValidationState,
    },
    Conflict {
        path: String,
        reason: String,
        base_text: String,
        current_text: String,
        proposed_text: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EditorChange {
    from: u32,
    to: u32,
    insert: String,
}

struct Review {
    agent_id: u32,
    gate: WorkspaceTrustGate,
    change: JudgedChange,
    steps: Vec<StepReport>,
    status: ReviewStatus,
    stopped: Option<StopReason>,
    preview: Option<PreviewSnapshot>,
    /// Files of the change already applied; the review closes once every file is.
    accepted: BTreeSet<String>,
}

impl Review {
    fn summary(&self) -> ReviewSummary {
        let evidence = &self.change.evidence.evidence;
        ReviewSummary {
            review_id: self.change.review_id.clone(),
            agent_id: self.agent_id,
            verdict: evidence.verdict,
            attempts: self.change.attempts,
            status: self.status,
            stopped: self.stopped.map(|reason| format!("{reason:?}")),
            files: self.change.files.clone(),
            steps: self
                .steps
                .iter()
                .map(|step| StepSummary {
                    kind: step.kind,
                    passed: step.passed,
                    exit_code: step.record.exit_code,
                    duration_ms: step.record.duration_ms,
                })
                .collect(),
            diagnostics: evidence
                .diagnostics
                .items
                .iter()
                .map(|item| DiagnosticSummary {
                    file_path: item.file_path.clone(),
                    line: item.line_col.line + 1,
                    column: item.line_col.col_utf16 + 1,
                    source: item.source,
                    code: item.code.clone(),
                    message: item.message.clone(),
                })
                .collect(),
            preview: self.preview.clone(),
        }
    }
}

/// Proposals of an agent since its last verification, and its correction loop.
#[derive(Default)]
struct Cycle {
    proposals: BTreeMap<String, ProposedEdit>,
    pending: bool,
    evaluating: bool,
    correction: CorrectionLoop,
    runs: u32,
}

/// An accept the Host planned: the only agent transaction it will apply to that document.
struct PendingAccept {
    review_id: String,
    path: String,
    base_revision: u32,
    plan: AcceptPlan,
}

/// The evidence a document's ranges were last verified with, for re-verification.
struct StaleDocument {
    gate: WorkspaceTrustGate,
    path: String,
}

#[derive(Default)]
struct Inner {
    cycles: HashMap<u32, Cycle>,
    reviews: BTreeMap<String, Review>,
    accepts: HashMap<(String, u32), PendingAccept>,
    stale: HashMap<(String, u32), StaleDocument>,
    reverifier: StaleReverifier,
    next_id: u64,
}

pub(crate) struct JudgeState {
    authority: Arc<HostAuthority>,
    inner: Mutex<Inner>,
    wake: Condvar,
    sink: Mutex<Option<JudgeSink>>,
}

impl JudgeState {
    pub(crate) fn new() -> Self {
        Self {
            authority: Arc::new(HostAuthority::new().expect("Host entropy is required")),
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
            sink: Mutex::new(None),
        }
    }

    pub(crate) fn set_sink(&self, sink: JudgeSink) {
        if let Ok(mut slot) = self.sink.lock() {
            *slot = Some(sink);
        }
    }

    fn emit(&self, event: JudgeEvent) {
        let sink = self.sink.lock().ok().and_then(|sink| sink.clone());
        if let Some(sink) = sink {
            sink(event);
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, String> {
        self.inner
            .lock()
            .map_err(|_| "judge lock poisoned".to_string())
    }
}

/// An agent wrote a file: keep its latest proposal per path until the turn ends.
pub(crate) fn record_proposal(state: &DesktopAppState, agent_id: u32, edit: &ProposedEdit) {
    if let Ok(mut inner) = state.judge.lock() {
        let cycle = inner.cycles.entry(agent_id).or_default();
        cycle.proposals.insert(edit.path.clone(), edit.clone());
        cycle.pending = true;
    }
}

/// Whether an agent's turn ended with proposals the Judge has not verified yet.
pub(crate) fn take_pending_turn(state: &DesktopAppState, agent_id: u32) -> bool {
    let Ok(mut inner) = state.judge.lock() else {
        return false;
    };
    match inner.cycles.get_mut(&agent_id) {
        Some(cycle) if cycle.pending && !cycle.evaluating => {
            cycle.pending = false;
            cycle.evaluating = true;
            true
        }
        _ => false,
    }
}

pub(crate) fn forget_agent(state: &DesktopAppState, agent_id: u32) {
    if let Ok(mut inner) = state.judge.lock() {
        inner.cycles.remove(&agent_id);
    }
}

/// Verifies the proposals of `agent_id`'s last turn, and either sends the failure back to the
/// agent or makes the result a review. `prompt` reaches the agent.
pub(crate) fn evaluate_turn(
    state: &DesktopAppState,
    agent_id: u32,
    gate: &WorkspaceTrustGate,
    prompt: &dyn Fn(String) -> Result<(), String>,
) {
    let result = evaluate(state, agent_id, gate, prompt);
    let again = state.judge.lock().is_ok_and(|mut inner| {
        let cycle = inner.cycles.entry(agent_id).or_default();
        cycle.evaluating = false;
        std::mem::take(&mut cycle.pending)
    });
    if let Err(reason) = result {
        state.judge.emit(JudgeEvent::Error { agent_id, reason });
    }
    if again && take_pending_turn(state, agent_id) {
        evaluate_turn(state, agent_id, gate, prompt);
    }
}

fn evaluate(
    state: &DesktopAppState,
    agent_id: u32,
    gate: &WorkspaceTrustGate,
    prompt: &dyn Fn(String) -> Result<(), String>,
) -> Result<(), String> {
    let root = gate.workspace_root().to_path_buf();
    let (proposals, attempt, run) = {
        let mut inner = state.judge.lock()?;
        let cycle = inner.cycles.entry(agent_id).or_default();
        cycle.runs += 1;
        (
            cycle.proposals.values().cloned().collect::<Vec<_>>(),
            cycle.correction.attempt(),
            cycle.runs,
        )
    };
    if proposals.is_empty() {
        return Ok(());
    }
    let task_id = format!(
        "judge-{}-{agent_id}-{run}",
        super::agent::session_start_now()
    );
    let file_proposals: Vec<FileProposal> = proposals
        .iter()
        .map(|edit| FileProposal {
            path: edit.path.clone(),
            old_text: edit.old_text.clone(),
            new_text: edit.new_text.clone(),
        })
        .collect();
    let Some(changeset) = changeset_from_proposals(&task_id, &task_id, 0, &file_proposals) else {
        return Ok(());
    };
    let (texts, buffers) = workspace_texts(state, &root, &changeset.affected_files)?;
    let review_id = format!("review-{task_id}");
    let compiled = match AstChangeSetCompiler::compile_changeset(&changeset, |path| {
        texts.get(path).map(|text| Rope::from_str(text))
    }) {
        Ok(compiled) => compiled,
        // The user edited what the agent changed: nothing to verify, show it for review.
        Err(error) => return Err(format!("The proposed change no longer applies: {error}")),
    };
    state
        .judge
        .emit(JudgeEvent::Verifying { agent_id, attempt });
    let pipeline = pipeline(state, gate)?;
    let impacted = impacted(state, &compiled);
    let outcome = pipeline
        .evaluate_changeset(&compiled, attempt, &buffers, &impacted, &mut |progress| {
            if let JudgeProgress::FirstVerdict {
                state: verdict,
                elapsed,
            } = progress
            {
                state.judge.emit(JudgeEvent::FirstVerdict {
                    agent_id,
                    attempt,
                    state: verdict,
                    elapsed_ms: elapsed.as_millis() as u64,
                });
            }
        })
        .map_err(|error| format!("The Judge could not verify the change: {error}"))?;
    let decision = {
        let mut inner = state.judge.lock()?;
        inner
            .cycles
            .entry(agent_id)
            .or_default()
            .correction
            .record(&outcome)
    };
    let stopped = match decision {
        CorrectionDecision::Retry {
            next_attempt,
            feedback,
        } => {
            prompt(feedback)?;
            state.judge.emit(JudgeEvent::Retrying {
                agent_id,
                attempt: next_attempt,
            });
            return Ok(());
        }
        CorrectionDecision::Stop(reason) => Some(reason),
        CorrectionDecision::AwaitReview => None,
    };
    let change = JudgedChange {
        review_id: review_id.clone(),
        changeset,
        files: compiled
            .files
            .iter()
            .map(|file| ReviewFile::from_compiled(&texts[&file.file_path], file))
            .collect(),
        evidence: outcome.evidence,
        attempts: attempt,
    };
    let review = Review {
        agent_id,
        gate: gate.clone(),
        change,
        steps: outcome.steps,
        status: ReviewStatus::Pending,
        stopped,
        preview: outcome.preview,
        accepted: BTreeSet::new(),
    };
    let summary = review.summary();
    {
        let mut inner = state.judge.lock()?;
        // The next turn of this agent starts a new change.
        if let Some(cycle) = inner.cycles.get_mut(&agent_id) {
            cycle.proposals.clear();
            cycle.correction = CorrectionLoop::new();
        }
        inner.reviews.insert(review_id, review);
    }
    state
        .judge
        .emit(JudgeEvent::ReviewReady { review: summary });
    Ok(())
}

/// Texts by workspace-relative path.
type Texts = BTreeMap<String, String>;

/// Current text of each affected file (live rope, else disk), and the unsaved buffers of the
/// workspace.
fn workspace_texts(
    state: &DesktopAppState,
    root: &Path,
    paths: &[String],
) -> Result<(Texts, Texts), String> {
    let bridge = state.bridge.lock().map_err(|_| "document lock poisoned")?;
    let mut buffers = BTreeMap::new();
    for session in bridge.document_service().sessions() {
        let Some(relative) = session
            .disk_path()
            .and_then(|path| path.strip_prefix(root).ok())
        else {
            continue;
        };
        if session.has_unsaved_changes() {
            buffers.insert(
                relative.to_string_lossy().replace('\\', "/"),
                session.text(),
            );
        }
    }
    let mut texts = BTreeMap::new();
    for path in paths {
        let text = match live_session(&bridge, root, path) {
            Some((_, _, text)) => text,
            None => match std::fs::read_to_string(root.join(path)) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(format!("{path}: {error}")),
            },
        };
        texts.insert(path.clone(), text);
    }
    Ok((texts, buffers))
}

/// The newest open session of `path`: its id, generation and text.
fn live_session(
    bridge: &DesktopBinaryEditorBridge,
    root: &Path,
    path: &str,
) -> Option<(String, u32, String)> {
    let target = root.join(path);
    bridge
        .document_service()
        .sessions()
        .filter(|session| session.disk_path() == Some(target.as_path()))
        .max_by_key(|session| session.session_generation())
        .map(|session| {
            (
                session.document_id().to_string(),
                session.session_generation(),
                session.text(),
            )
        })
}

fn pipeline(state: &DesktopAppState, gate: &WorkspaceTrustGate) -> Result<JudgePipeline, String> {
    let root = gate.workspace_root();
    let checkpoints =
        CheckpointManager::open_or_shadow(root, &state.app_data_dir.join("shadow-repos"))
            .map_err(|e| e.to_string())?
            .with_worktrees_dir(&state.app_data_dir.join("judge-worktrees"))
            .map_err(|e| e.to_string())?;
    let limits = ResourceLimits {
        wall_timeout: STEP_TIMEOUT,
        ..ResourceLimits::default()
    };
    // Without Node.js or the workspace's tools nothing can be checked: the change stays `🟡`.
    let plan = NodeToolchain::from_path()
        .and_then(|toolchain| node_verification_plan(gate, &toolchain, limits))
        .unwrap_or(VerificationPlan {
            vitest: None,
            biome: None,
            typescript: None,
            full_suite: false,
            read_only_paths: Vec::new(),
            limits,
        });
    Ok(JudgePipeline::new(
        Arc::new(checkpoints),
        crate::commands::agent::shared_sandbox_runner(state)?,
        Arc::clone(&state.judge.authority),
        plan,
    ))
}

/// Impacted tests from the workspace index, when it is built.
fn impacted(state: &DesktopAppState, compiled: &CompiledChangeSet) -> Vec<String> {
    let Ok(engine) = state.context_engine.lock() else {
        return Vec::new();
    };
    let Some(engine) = engine.as_ref() else {
        return Vec::new();
    };
    let files: Vec<_> = compiled
        .files
        .iter()
        .map(|file| {
            (
                file.file_path.as_str(),
                file.result_text.as_str(),
                file.result_spans(),
            )
        })
        .collect();
    impacted_tests(engine.impact_analyzer(), &files)
}

pub(crate) fn review(state: &DesktopAppState, review_id: &str) -> Result<ReviewSummary, String> {
    let inner = state.judge.lock()?;
    inner
        .reviews
        .get(review_id)
        .map(Review::summary)
        .ok_or_else(|| "This review is closed".to_string())
}

pub(crate) fn reviews(state: &DesktopAppState) -> Result<Vec<ReviewSummary>, String> {
    let inner = state.judge.lock()?;
    Ok(inner
        .reviews
        .values()
        .filter(|review| review.status == ReviewStatus::Pending)
        .map(Review::summary)
        .collect())
}

/// Plans accepting `path` of a review into the open document `(document_id, generation)`.
pub(crate) fn plan_accept_into(
    state: &DesktopAppState,
    review_id: &str,
    path: &str,
    document_id: &str,
    session_generation: u32,
) -> Result<AcceptResponse, String> {
    let bridge = state.bridge.lock().map_err(|_| "document lock poisoned")?;
    let mut inner = state.judge.lock()?;
    let review = inner
        .reviews
        .get(review_id)
        .filter(|review| review.status == ReviewStatus::Pending)
        .ok_or("This review is closed")?;
    review
        .gate
        .authorize(host_core::GatedCapability::WorkspaceLocalBinary)
        .map_err(|e| e.to_string())?;
    let root = review.gate.workspace_root();
    let session = bridge
        .document_service()
        .session(document_id, session_generation)
        .map_err(|e| e.to_string())?;
    if session.disk_path() != Some(root.join(path).as_path()) {
        return Err("The open document is not the reviewed file".into());
    }
    let base_text = review
        .change
        .files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.base_text.clone())
        .ok_or("This file is not part of the review")?;
    match plan_accept(&state.judge.authority, &review.change, path, session.rope()) {
        Ok(plan) => {
            let state_after = plan
                .ranges
                .first()
                .map_or(HiloValidationState::ReviewYellow, |(_, state)| *state);
            let response = AcceptResponse::Apply {
                task_id: review.change.changeset.task_id.clone(),
                base_revision: session.interactive_revision(),
                changes: plan
                    .changes
                    .iter()
                    .map(|change| EditorChange {
                        from: change.range_utf16.start.as_u32(),
                        to: change.range_utf16.end.as_u32(),
                        insert: change.inserted_text.clone(),
                    })
                    .collect(),
                state: state_after,
            };
            inner.accepts.insert(
                (document_id.to_string(), session_generation),
                PendingAccept {
                    review_id: review_id.to_string(),
                    path: path.to_string(),
                    base_revision: session.interactive_revision(),
                    plan,
                },
            );
            Ok(response)
        }
        Err(AcceptRefusal::Conflict {
            path,
            reason,
            current_text,
            proposed_text,
        }) => Ok(AcceptResponse::Conflict {
            path,
            reason,
            base_text,
            current_text,
            proposed_text,
        }),
        Err(refusal) => Err(refusal.to_string()),
    }
}

/// Checks a transaction before host-core applies it. A user's keystroke passes. An agent
/// transaction passes only if it is the planned accept of this document, from the exact
/// revision it was planned on, producing exactly the planned content.
pub(crate) fn authorize_transaction(
    state: &DesktopAppState,
    documents: &DocumentService,
    tx: &EditorTransaction,
) -> Result<Option<(String, String, AcceptPlan)>, String> {
    let TransactionOrigin::Agent { task_id } = &tx.origin else {
        return Ok(None);
    };
    let mut inner = state.judge.lock()?;
    let key = (tx.document_id.clone(), tx.session_generation);
    let refused = || "An agent edit is applied only through an accepted Judge review".to_string();
    let pending = inner.accepts.remove(&key).ok_or_else(refused)?;
    let review_matches = inner.reviews.get(&pending.review_id).is_some_and(|review| {
        review.status == ReviewStatus::Pending && &review.change.changeset.task_id == task_id
    });
    let session = documents
        .session(&tx.document_id, tx.session_generation)
        .map_err(|e| e.to_string())?;
    if !review_matches
        || tx.base_revision != pending.base_revision
        || session.interactive_revision() != pending.base_revision
        || !matches_plan(&pending.plan, session.rope(), &tx.changes)
    {
        return Err(refused());
    }
    Ok(Some((pending.review_id, pending.path, pending.plan)))
}

/// After host-core applied `tx`: an accepted change records its ranges and closes its review
/// once every file is in; a user edit that made ranges stale schedules a re-verification.
pub(crate) fn after_transaction(
    state: &DesktopAppState,
    documents: &mut DocumentService,
    tx: &EditorTransaction,
    accepted: Option<(String, String, AcceptPlan)>,
) {
    let key = (tx.document_id.clone(), tx.session_generation);
    let Ok(session) = documents.session_mut(&tx.document_id, tx.session_generation) else {
        return;
    };
    let Some((review_id, path, plan)) = accepted else {
        let stale = session
            .validation_ranges()
            .iter()
            .any(|range| range.state == HiloValidationState::StaleOrange);
        if stale {
            if let Ok(mut inner) = state.judge.lock() {
                if inner.stale.contains_key(&key) {
                    inner.reverifier.note_edit(&tx.document_id, Instant::now());
                    drop(inner);
                    state.judge.wake.notify_all();
                }
            }
        }
        return;
    };
    // The new ranges replace the ones they overlap; the others were moved by host-core.
    let mut ranges: Vec<(Utf16Span, HiloValidationState)> = session
        .validation_ranges()
        .iter()
        .filter(|range| {
            !plan.ranges.iter().any(|(span, _)| {
                range.span_utf16.start < span.end && span.start < range.span_utf16.end
            })
        })
        .map(|range| (range.span_utf16, range.state))
        .collect();
    ranges.extend(plan.ranges.iter().copied());
    ranges.sort_by_key(|(span, _)| span.start);
    let _ = session.set_validation_ranges(ranges);
    let stale = plan
        .ranges
        .iter()
        .any(|(_, state)| *state == HiloValidationState::StaleOrange);
    let decorations = decorations(session);
    let content_hash = session.content_blake3_hash().to_string();
    let mut closed = false;
    if let Ok(mut inner) = state.judge.lock() {
        let gate = inner
            .reviews
            .get(&review_id)
            .map(|review| review.gate.clone());
        if let Some(gate) = gate {
            inner.stale.insert(
                key,
                StaleDocument {
                    gate,
                    path: path.clone(),
                },
            );
            if stale {
                inner.reverifier.verify_now(&tx.document_id, Instant::now());
            }
        }
        if let Some(review) = inner.reviews.get_mut(&review_id) {
            review.accepted.insert(path);
            closed = review.accepted.len() == review.change.files.len();
        }
        if closed {
            inner.reviews.remove(&review_id);
        }
    }
    state.judge.wake.notify_all();
    state.judge.emit(JudgeEvent::States {
        document_id: tx.document_id.clone(),
        session_generation: tx.session_generation,
        content_hash,
        decorations,
    });
    if closed {
        state.judge.emit(JudgeEvent::ReviewClosed {
            review_id,
            status: ReviewStatus::Accepted,
        });
    }
}

pub(crate) fn reject(state: &DesktopAppState, review_id: &str) -> Result<(), String> {
    let removed = state.judge.lock()?.reviews.remove(review_id);
    if removed.is_none() {
        return Err("This review is closed".into());
    }
    state.judge.emit(JudgeEvent::ReviewClosed {
        review_id: review_id.to_string(),
        status: ReviewStatus::Rejected,
    });
    Ok(())
}

/// Sends the user's request and the evidence to the agent; its next turn is a new change.
pub(crate) fn request_adjustment(
    state: &DesktopAppState,
    review_id: &str,
    note: &str,
    prompt: &dyn Fn(u32, String) -> Result<(), String>,
) -> Result<(), String> {
    if note.trim().is_empty() || note.len() > 8 * 1024 {
        return Err("Describe the adjustment (8 KiB at most)".into());
    }
    let (agent_id, text) = {
        let inner = state.judge.lock()?;
        let review = inner
            .reviews
            .get(review_id)
            .ok_or("This review is closed")?;
        (review.agent_id, adjustment_prompt(note, &review.change))
    };
    prompt(agent_id, text)?;
    state.judge.lock()?.reviews.remove(review_id);
    state.judge.emit(JudgeEvent::ReviewClosed {
        review_id: review_id.to_string(),
        status: ReviewStatus::Adjusting,
    });
    Ok(())
}

pub(crate) fn decorations(session: &host_core::DocumentSession) -> Vec<StateDecoration> {
    let rope = session.rope();
    session
        .validation_ranges()
        .iter()
        .map(|range| {
            let line = |offset: u32| {
                let offset = (offset as usize).min(rope.len_bytes());
                rope.byte_to_line(offset) as u32 + 1
            };
            let end = range
                .span_utf8
                .end
                .as_u32()
                .saturating_sub(1)
                .max(range.span_utf8.start.as_u32());
            StateDecoration {
                from_line: line(range.span_utf8.start.as_u32()),
                to_line: line(end),
                state: range.state,
                snapshot_hash: session.content_blake3_hash().to_string(),
            }
        })
        .collect()
}

pub(crate) fn document_states(
    state: &DesktopAppState,
    document_id: &str,
    session_generation: u32,
) -> Result<Vec<StateDecoration>, String> {
    let bridge = state.bridge.lock().map_err(|_| "document lock poisoned")?;
    let session = bridge
        .document_service()
        .session(document_id, session_generation)
        .map_err(|e| e.to_string())?;
    Ok(decorations(session))
}

/// A save verifies the stale ranges of the document at once.
pub(crate) fn document_saved(state: &DesktopAppState, document_id: &str) {
    if let Ok(mut inner) = state.judge.lock() {
        inner.reverifier.note_save(document_id, Instant::now());
    }
    state.judge.wake.notify_all();
}

/// Verifies the stale documents whose idle time ran out. Returns when the next one is due.
pub(crate) fn reverify_due(state: &DesktopAppState, now: Instant) -> Option<Instant> {
    let due = state.judge.lock().ok()?.reverifier.take_due(now);
    for document_id in due {
        let targets: Vec<(u32, WorkspaceTrustGate, String)> = state
            .judge
            .lock()
            .map(|inner| {
                inner
                    .stale
                    .iter()
                    .filter(|((id, _), _)| *id == document_id)
                    .map(|((_, generation), stale)| {
                        (*generation, stale.gate.clone(), stale.path.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (generation, gate, path) in targets {
            if let Err(error) = reverify(state, &document_id, generation, &gate, &path) {
                tracing::warn!(%error, document_id, "re-verification failed");
            }
        }
    }
    state.judge.lock().ok()?.reverifier.next_due()
}

fn reverify(
    state: &DesktopAppState,
    document_id: &str,
    generation: u32,
    gate: &WorkspaceTrustGate,
    path: &str,
) -> Result<(), String> {
    let root = gate.workspace_root().to_path_buf();
    let (text, spans, hash) = {
        let bridge = state.bridge.lock().map_err(|_| "document lock poisoned")?;
        let session = bridge
            .document_service()
            .session(document_id, generation)
            .map_err(|e| e.to_string())?;
        let spans: Vec<_> = session
            .validation_ranges()
            .iter()
            .filter(|range| range.state == HiloValidationState::StaleOrange)
            .map(|range| range.span_utf8)
            .collect();
        if spans.is_empty() {
            return Ok(());
        }
        (
            session.text(),
            spans,
            session.content_blake3_hash().to_string(),
        )
    };
    let (_, mut buffers) = workspace_texts(state, &root, &[])?;
    buffers.remove(path);
    let impacted = state
        .context_engine
        .lock()
        .ok()
        .and_then(|engine| {
            engine.as_ref().map(|engine| {
                impacted_tests(
                    engine.impact_analyzer(),
                    &[(path, text.as_str(), spans.clone())],
                )
            })
        })
        .unwrap_or_default();
    let run = {
        let mut inner = state.judge.lock()?;
        inner.next_id += 1;
        inner.next_id
    };
    let checkpoint_task = format!("reverify-{}-{run}", super::agent::session_start_now());
    let files = BTreeMap::from([(path.to_string(), text)]);
    let outcome = pipeline(state, gate)?
        .evaluate(
            &Verification {
                checkpoint_task: &checkpoint_task,
                task_id: &checkpoint_task,
                files: &files,
                buffers: &buffers,
                impacted_tests: &impacted,
            },
            &mut |_| {},
        )
        .map_err(|e| e.to_string())?;
    let mut bridge = state.bridge.lock().map_err(|_| "document lock poisoned")?;
    let session = bridge
        .documents_mut()
        .session_mut(document_id, generation)
        .map_err(|e| e.to_string())?;
    // Dropped if the document changed while the Judge ran: the ranges stay stale.
    let Some(ranges) = reverified_ranges(
        session.validation_ranges(),
        &outcome.evidence.evidence,
        path,
        &hash,
    )
    .filter(|_| session.content_blake3_hash() == hash) else {
        return Ok(());
    };
    session
        .set_validation_ranges(ranges)
        .map_err(|e| e.to_string())?;
    let decorations = decorations(session);
    drop(bridge);
    state.judge.emit(JudgeEvent::States {
        document_id: document_id.to_string(),
        session_generation: generation,
        content_hash: hash,
        decorations,
    });
    Ok(())
}

/// Runs the re-verifications as they fall due, for the life of the app.
pub(crate) fn reverify_loop(state: &DesktopAppState) {
    loop {
        reverify_due(state, Instant::now());
        let Ok(inner) = state.judge.inner.lock() else {
            return;
        };
        // Read under the lock that every `note_*` takes: a wake-up cannot be missed.
        let wait = inner
            .reverifier
            .next_due()
            .map_or(Duration::from_secs(3600), |at| {
                at.saturating_duration_since(Instant::now())
            });
        let _ = state.judge.wake.wait_timeout(inner, wait);
    }
}

async fn on_worker<T: Send + 'static>(
    app: tauri::AppHandle,
    work: impl FnOnce(&DesktopAppState) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || work(&app.state::<DesktopAppState>()))
        .await
        .map_err(|error| format!("Judge task failed: {error}"))?
}

#[tauri::command]
pub async fn judge_reviews_command(app: tauri::AppHandle) -> Result<Vec<ReviewSummary>, String> {
    on_worker(app, reviews).await
}

#[tauri::command]
pub async fn judge_review_command(
    app: tauri::AppHandle,
    review_id: String,
) -> Result<ReviewSummary, String> {
    on_worker(app, move |state| review(state, &review_id)).await
}

/// `[✓ Accept]` for one file, open in the editor as `(document_id, session_generation)`.
#[tauri::command]
pub async fn judge_accept_command(
    app: tauri::AppHandle,
    review_id: String,
    path: String,
    document_id: String,
    session_generation: u32,
) -> Result<AcceptResponse, String> {
    on_worker(app, move |state| {
        plan_accept_into(state, &review_id, &path, &document_id, session_generation)
    })
    .await
}

/// `[✕ Reject]`: nothing is applied.
#[tauri::command]
pub async fn judge_reject_command(app: tauri::AppHandle, review_id: String) -> Result<(), String> {
    on_worker(app, move |state| reject(state, &review_id)).await
}

/// `[Request Adjustment]`: the user's words and the evidence go to the agent.
#[tauri::command]
pub async fn judge_adjust_command(
    app: tauri::AppHandle,
    review_id: String,
    note: String,
) -> Result<(), String> {
    on_worker(app, move |state| {
        request_adjustment(state, &review_id, &note, &|agent_id, text| {
            crate::commands::agent::prompt_agent(state, agent_id, text)
        })
    })
    .await
}

#[tauri::command]
pub async fn judge_document_states_command(
    app: tauri::AppHandle,
    document_id: String,
    session_generation: u32,
) -> Result<Vec<StateDecoration>, String> {
    on_worker(app, move |state| {
        document_states(state, &document_id, session_generation)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::documents::apply_transaction;
    use crate::commands::workspace::load_workspace;
    use crate::state::test_state;
    use host_core::{
        encode_editor_transaction_ipc, EditorSelection, Evidence, EvidenceArtifact,
        ExecutedCommandRecord, PreviewRuntimeReport, QualityDiagnosticsSummary, TextChange,
        Utf16CodeUnitOffset, CONTRACT_SCHEMA_VERSION, TESTED_FILE_ARTIFACT_KIND,
    };

    const BASE: &str = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
    const PROPOSED: &str =
        "export function add(a: number, b: number): number {\n  return b + a;\n}\n";

    struct Setup {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        state: DesktopAppState,
        events: Arc<Mutex<Vec<JudgeEvent>>>,
    }

    /// A trusted workspace with `src/a.ts` open in the editor and a pending review of a change
    /// to it, verified `🟢` (the evidence is signed by this Host, as the pipeline signs it).
    fn setup() -> Setup {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/a.ts"), BASE).unwrap();
        let state = test_state(data.path());
        load_workspace(&state, workspace.path().to_str().unwrap()).unwrap();
        let gate = state.trust_gate.lock().unwrap().clone().unwrap();
        gate.grant().unwrap();
        let root = gate.workspace_root().to_path_buf();
        state
            .bridge
            .lock()
            .unwrap()
            .open_document_with_editor_text_binary("src/a.ts", 1, root.join("src/a.ts"), BASE)
            .unwrap();
        let changeset = changeset_from_proposals(
            "task-1",
            "task-1",
            0,
            &[FileProposal {
                path: "src/a.ts".into(),
                old_text: BASE.into(),
                new_text: PROPOSED.into(),
            }],
        )
        .unwrap();
        let compiled =
            AstChangeSetCompiler::compile_changeset(&changeset, |_| Some(Rope::from_str(BASE)))
                .unwrap();
        let digest = compiled.files[0].result_content_hash.clone();
        let evidence = Evidence {
            schema_version: CONTRACT_SCHEMA_VERSION,
            evidence_id: "e".into(),
            task_id: "task-1".into(),
            tested_snapshot_hash: digest.clone(),
            checkpoint_ref: "refs/hilo/checkpoints/task-1.1/1".into(),
            commands: vec![ExecutedCommandRecord {
                command: "node".into(),
                args: vec![],
                exit_code: 0,
                duration_ms: 1,
                impacted_tests_only: true,
            }],
            diagnostics: QualityDiagnosticsSummary::default(),
            preview_report: PreviewRuntimeReport {
                inspected_url: String::new(),
                console_error_count: 0,
                uncaught_exception_count: 0,
                failed_network_request_count: 0,
                crashed: false,
                dom_snapshot_hash: None,
            },
            artifacts: vec![EvidenceArtifact {
                name: "src/a.ts".into(),
                kind: TESTED_FILE_ARTIFACT_KIND.into(),
                digest_blake3: digest,
                byte_size: PROPOSED.len() as u32,
            }],
            verdict: HiloValidationState::ValidatedGreen,
        };
        let change = JudgedChange {
            review_id: "review-1".into(),
            changeset,
            files: vec![ReviewFile::from_compiled(BASE, &compiled.files[0])],
            evidence: state.judge.authority.sign_evidence(evidence).unwrap(),
            attempts: 1,
        };
        state.judge.lock().unwrap().reviews.insert(
            "review-1".into(),
            Review {
                agent_id: 7,
                gate,
                change,
                steps: Vec::new(),
                status: ReviewStatus::Pending,
                stopped: None,
                preview: None,
                accepted: BTreeSet::new(),
            },
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        state
            .judge
            .set_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
        Setup {
            _dirs: (data, workspace),
            state,
            events,
        }
    }

    fn payload(base_revision: u32, changes: &[EditorChange], origin: TransactionOrigin) -> Vec<u8> {
        // CodeMirror's changes are in the coordinates of the text before the transaction;
        // host-core applies them one after another (the editor converts them the same way).
        let mut shift: i64 = 0;
        let changes = changes
            .iter()
            .map(|change| {
                let at = |offset: u32| Utf16CodeUnitOffset::new((i64::from(offset) + shift) as u32);
                let next = TextChange {
                    range_utf16: Utf16Span::new(at(change.from), at(change.to)),
                    range_utf8: None,
                    inserted_text: change.insert.clone(),
                };
                shift += change.insert.encode_utf16().count() as i64
                    - i64::from(change.to - change.from);
                next
            })
            .collect();
        encode_editor_transaction_ipc(&EditorTransaction {
            schema_version: CONTRACT_SCHEMA_VERSION,
            document_id: "src/a.ts".into(),
            session_generation: 1,
            base_revision,
            op_id: format!("op-{base_revision}"),
            changes,
            selection: EditorSelection::default(),
            origin,
            ime_composing: false,
        })
        .unwrap()
    }

    fn text(state: &DesktopAppState) -> String {
        state
            .bridge
            .lock()
            .unwrap()
            .document_service()
            .session("src/a.ts", 1)
            .unwrap()
            .text()
    }

    fn agent() -> TransactionOrigin {
        TransactionOrigin::Agent {
            task_id: "task-1".into(),
        }
    }

    #[test]
    fn an_agent_edit_without_an_accepted_review_never_reaches_the_document() {
        let setup = setup();
        let forged = [EditorChange {
            from: 0,
            to: 0,
            insert: "// injected\n".into(),
        }];
        let error = apply_transaction(&setup.state, &payload(0, &forged, agent())).unwrap_err();
        assert!(error.contains("accepted Judge review"), "{error}");
        assert_eq!(text(&setup.state), BASE);
    }

    #[test]
    fn an_accept_applies_only_the_planned_content_and_marks_it_validated() {
        let setup = setup();
        let AcceptResponse::Apply {
            changes,
            base_revision,
            state: planned,
            ..
        } = plan_accept_into(&setup.state, "review-1", "src/a.ts", "src/a.ts", 1).unwrap()
        else {
            panic!("no conflict expected")
        };
        assert_eq!(planned, HiloValidationState::ValidatedGreen);

        // The webview changing what it was told to dispatch is refused, and spends the plan.
        let mut tampered = changes.clone();
        tampered[0].insert.push_str("// extra\n");
        assert!(
            apply_transaction(&setup.state, &payload(base_revision, &tampered, agent())).is_err()
        );
        assert_eq!(text(&setup.state), BASE);
        assert!(
            apply_transaction(&setup.state, &payload(base_revision, &changes, agent())).is_err()
        );

        let AcceptResponse::Apply {
            changes,
            base_revision,
            ..
        } = plan_accept_into(&setup.state, "review-1", "src/a.ts", "src/a.ts", 1).unwrap()
        else {
            panic!()
        };
        apply_transaction(&setup.state, &payload(base_revision, &changes, agent())).unwrap();
        assert_eq!(text(&setup.state), PROPOSED);
        let states = document_states(&setup.state, "src/a.ts", 1).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].state, HiloValidationState::ValidatedGreen);
        assert_eq!((states[0].from_line, states[0].to_line), (1, 3));
        assert!(
            review(&setup.state, "review-1").is_err(),
            "the review is closed"
        );
        let events = setup.events.lock().unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, JudgeEvent::States { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            JudgeEvent::ReviewClosed {
                status: ReviewStatus::Accepted,
                ..
            }
        )));
    }

    #[test]
    fn a_user_edit_of_the_target_turns_the_accept_into_a_conflict() {
        let setup = setup();
        let start = BASE.find("a + b").unwrap() as u32;
        let user = [EditorChange {
            from: start,
            to: start + 5,
            insert: "a + b + 0".into(),
        }];
        apply_transaction(&setup.state, &payload(0, &user, TransactionOrigin::User)).unwrap();
        let edited = text(&setup.state);
        match plan_accept_into(&setup.state, "review-1", "src/a.ts", "src/a.ts", 1).unwrap() {
            AcceptResponse::Conflict {
                base_text,
                current_text,
                proposed_text,
                ..
            } => {
                assert_eq!(base_text, BASE);
                assert_eq!(current_text, edited);
                assert_eq!(proposed_text, PROPOSED);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(text(&setup.state), edited);
        // A review stays open after a conflict: the user can still reject it or ask again.
        assert!(review(&setup.state, "review-1").is_ok());
        reject(&setup.state, "review-1").unwrap();
        assert!(review(&setup.state, "review-1").is_err());
    }
}
