//! LOT-015 blocking tests, with real processes only: a real Git repository and its invisible
//! checkpoints, the native OS sandbox, and the repository's own Vitest, Biome and TypeScript
//! (`pnpm install` must have run, and `node` must be on `PATH`).
//!
//! Unix only: on Windows the Judge plans no tool run (worktrees there are copies without the
//! installed packages, LOT-011), which `the_windows_plan_runs_no_tool` checks instead.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use context_engine::ImpactAnalyzer;
use host_core::{
    ropey::Rope, CheckpointManager, DocumentService, EditorSelection, EditorTransaction,
    HiloValidationState, TextChange, TransactionOrigin, Utf16CodeUnitOffset, Utf16Span,
    WorkspaceTrustGate, CONTRACT_SCHEMA_VERSION,
};
use judge_orchestrator::judge::*;
use judge_orchestrator::policy::HostAuthority;
use sandbox_runner::{native_sandbox, MemoryAuditSink, ResourceLimits};

const MONEY: &str = "src/money.ts";
const MONEY_BASE: &str = "export function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport function sub(a: number, b: number): number {\n  return a - b;\n}\n";
const MONEY_TEST: &str = "import { describe, expect, test } from \"vitest\";\nimport { add, sub } from \"./money\";\n\ndescribe(\"money\", () => {\n  test(\"adds\", () => {\n    expect(add(1, 2)).toBe(3);\n  });\n  test(\"subtracts\", () => {\n    expect(sub(5, 2)).toBe(3);\n  });\n});\n";
const FORMAT: &str = "export function label(name: string): string {\n  return `#${name}`;\n}\n";
const FORMAT_TEST: &str = "import { expect, test } from \"vitest\";\nimport { label } from \"./format\";\n\ntest(\"labels\", () => {\n  expect(label(\"a\")).toBe(\"#a\");\n});\n";

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git must be installed to run the LOT-015 tests");
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

struct Fixture {
    _dirs: [tempfile::TempDir; 3],
    pipeline: JudgePipeline,
    authority: Arc<HostAuthority>,
    analyzer: ImpactAnalyzer,
}

fn fixture() -> Fixture {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let node_modules = repo_root.join("node_modules");
    assert!(
        node_modules.join("vitest").join("vitest.mjs").is_file(),
        "run `pnpm install` first: the Judge tests use the repository's Vitest, Biome and TypeScript"
    );
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let files = [
        (".gitignore", "node_modules\n"),
        ("package.json", "{ \"name\": \"fixture\", \"private\": true, \"type\": \"module\" }\n"),
        (
            "biome.json",
            "{ \"formatter\": { \"indentStyle\": \"space\", \"indentWidth\": 2 } }\n",
        ),
        (
            "tsconfig.json",
            "{ \"compilerOptions\": { \"strict\": true, \"noEmit\": true, \"target\": \"ES2022\", \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\", \"skipLibCheck\": true, \"types\": [] }, \"include\": [\"src\"] }\n",
        ),
        (MONEY, MONEY_BASE),
        ("src/money.test.ts", MONEY_TEST),
        ("src/format.ts", FORMAT),
        ("src/format.test.ts", FORMAT_TEST),
    ];
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (path, text) in files {
        std::fs::write(root.join(path), text).unwrap();
    }
    std::os::unix::fs::symlink(&node_modules, root.join("node_modules")).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "fixture"]);

    let worktrees = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let gate = WorkspaceTrustGate::open(trust.path().join("trust.json"), root).unwrap();
    gate.grant().unwrap();
    let toolchain = NodeToolchain::from_path().expect("node on PATH");
    let plan = node_verification_plan(
        &gate,
        &toolchain,
        ResourceLimits {
            wall_timeout: Duration::from_secs(120),
            ..ResourceLimits::default()
        },
    )
    .unwrap();
    assert!(plan.vitest.is_some() && plan.biome.is_some() && plan.typescript.is_some());
    let checkpoints = CheckpointManager::open(root)
        .unwrap()
        .with_worktrees_dir(worktrees.path())
        .unwrap();
    let runner = native_sandbox(Arc::new(MemoryAuditSink::default()))
        .expect("the native OS sandbox is required (fail closed)");
    let authority = Arc::new(HostAuthority::new().unwrap());
    let pipeline = JudgePipeline::new(
        Arc::new(checkpoints),
        Arc::from(runner),
        Arc::clone(&authority),
        plan,
    );
    let mut analyzer = ImpactAnalyzer::new();
    for (path, text) in files.iter().filter(|(path, _)| path.starts_with("src/")) {
        let rope = Rope::from_str(text);
        analyzer.index_file(path, &rope, &parse(&rope, path));
    }
    Fixture {
        _dirs: [workspace, worktrees, trust],
        pipeline,
        authority,
        analyzer,
    }
}

/// The agent's proposal for `money.ts` (as ACP delivers it), compiled against `current`.
fn compiled(
    task: &str,
    attempt: u32,
    current: &str,
    new_text: &str,
) -> (CompiledChangeSet, host_core::ChangeSet) {
    let changeset = changeset_from_proposals(
        &format!("{task}-{attempt}"),
        task,
        0,
        &[FileProposal {
            path: MONEY.into(),
            old_text: MONEY_BASE.into(),
            new_text: new_text.into(),
        }],
    )
    .unwrap();
    let compiled =
        AstChangeSetCompiler::compile_changeset(&changeset, |_| Some(Rope::from_str(current)))
            .unwrap();
    (compiled, changeset)
}

struct Attempt {
    outcome: JudgeOutcome,
    first_verdict: Option<(HiloValidationState, Duration)>,
    impacted: Vec<String>,
}

fn judge(fx: &Fixture, compiled: &CompiledChangeSet, attempt: u32) -> Attempt {
    let file = &compiled.files[0];
    let impacted = impacted_tests(
        &fx.analyzer,
        &[(MONEY, file.result_text.as_str(), file.result_spans())],
    );
    let mut first_verdict = None;
    let outcome = fx
        .pipeline
        .evaluate_changeset(
            compiled,
            attempt,
            &BTreeMap::new(),
            &impacted,
            &mut |progress| {
                if let JudgeProgress::FirstVerdict { state, elapsed } = progress {
                    first_verdict = Some((state, elapsed));
                }
            },
        )
        .unwrap();
    Attempt {
        outcome,
        first_verdict,
        impacted,
    }
}

/// What each check reported: shown when a verdict is not the expected one.
fn report(outcome: &JudgeOutcome) -> String {
    let mut text = String::new();
    for step in &outcome.steps {
        text.push_str(&format!(
            "{:?} passed={} exit={} {:?}\n",
            step.kind, step.passed, step.record.exit_code, step.record.args
        ));
        for item in &step.items {
            text.push_str(&format!(
                "    {} [{}] {}\n",
                item.file_path, item.code, item.message
            ));
        }
    }
    text
}

fn with_sub(body: &str) -> String {
    MONEY_BASE.replace("  return a - b;\n", body)
}

fn judged(
    changeset: host_core::ChangeSet,
    compiled: &CompiledChangeSet,
    outcome: &JudgeOutcome,
    attempts: u32,
) -> JudgedChange {
    JudgedChange {
        review_id: "review-1".into(),
        changeset,
        files: vec![ReviewFile::from_compiled(MONEY_BASE, &compiled.files[0])],
        evidence: outcome.evidence.clone(),
        attempts,
    }
}

fn document(text: &str) -> DocumentService {
    let mut documents = DocumentService::new_in_memory();
    documents
        .open_document_from_str(MONEY, 1, None, text)
        .unwrap();
    documents
}

fn transaction(
    documents: &DocumentService,
    changes: Vec<TextChange>,
    origin: TransactionOrigin,
) -> EditorTransaction {
    let session = documents.session(MONEY, 1).unwrap();
    EditorTransaction {
        schema_version: CONTRACT_SCHEMA_VERSION,
        document_id: MONEY.into(),
        session_generation: 1,
        base_revision: session.interactive_revision(),
        op_id: format!("op-{}", session.interactive_revision()),
        changes,
        selection: EditorSelection::default(),
        origin,
        ime_composing: false,
    }
}

/// A user keystroke replacing the first `find` in the document by `with`.
fn user_edit(documents: &mut DocumentService, find: &str, with: &str) {
    let text = documents.session(MONEY, 1).unwrap().text();
    let start = text[..text.find(find).unwrap()].encode_utf16().count() as u32;
    let end = start + find.encode_utf16().count() as u32;
    let tx = transaction(
        documents,
        vec![TextChange {
            range_utf16: Utf16Span::new(
                Utf16CodeUnitOffset::new(start),
                Utf16CodeUnitOffset::new(end),
            ),
            range_utf8: None,
            inserted_text: with.into(),
        }],
        TransactionOrigin::User,
    );
    documents.apply_transaction(tx).unwrap();
}

/// Accepting `change`: the Host plans it on the live rope, applies it as an agent edit and
/// records the ranges' states.
fn accept(
    fx: &Fixture,
    documents: &mut DocumentService,
    change: &JudgedChange,
) -> Result<AcceptPlan, AcceptRefusal> {
    let rope = documents.session(MONEY, 1).unwrap().rope().clone();
    let plan = plan_accept(&fx.authority, change, MONEY, &rope)?;
    assert!(matches_plan(&plan, &rope, &sequential_changes(&plan)));
    let tx = transaction(
        documents,
        sequential_changes(&plan),
        TransactionOrigin::Agent {
            task_id: change.changeset.task_id.clone(),
        },
    );
    documents.apply_transaction(tx).unwrap();
    let session = documents.session_mut(MONEY, 1).unwrap();
    assert_eq!(session.content_blake3_hash(), plan.result_content_hash);
    session.set_validation_ranges(plan.ranges.clone()).unwrap();
    Ok(plan)
}

/// Blocking (spec 4.5, test 1): a regression on attempt 1 is `🔴` with structured evidence, the
/// first verdict on the impacted tests comes in under 2 s, the fix on attempt 2 is `🟢` with
/// `tested_snapshot_hash == content_blake3_hash` once accepted.
#[test]
fn blocking_judge_loop_reports_red_then_green_within_the_retry_cap() {
    let fx = fixture();
    let mut correction = CorrectionLoop::new();

    let (first, _) = compiled("loop", 1, MONEY_BASE, &with_sub("  return a + b;\n"));
    let attempt1 = judge(&fx, &first, correction.attempt());
    assert_eq!(
        attempt1.impacted,
        ["src/money.test.ts"],
        "format.test.ts is not impacted"
    );
    assert_eq!(
        attempt1.outcome.verdict(),
        HiloValidationState::FailedRed,
        "{}",
        report(&attempt1.outcome)
    );
    let (state, elapsed) = attempt1.first_verdict.unwrap();
    assert_eq!(state, HiloValidationState::FailedRed);
    assert!(
        elapsed < Duration::from_secs(2),
        "first verdict after {elapsed:?}"
    );
    let evidence = &attempt1.outcome.evidence.evidence;
    assert!(evidence.commands[0].impacted_tests_only);
    assert_ne!(evidence.commands[0].exit_code, 0);
    assert!(evidence
        .diagnostics
        .items
        .iter()
        .any(|item| item.file_path == "src/money.test.ts" && item.message.contains("subtracts")),
        "{}",
        report(&attempt1.outcome)
    );
    assert!(fx.authority.verify_evidence(&attempt1.outcome.evidence));
    let CorrectionDecision::Retry {
        next_attempt,
        feedback,
    } = correction.record(&attempt1.outcome)
    else {
        panic!("a first failure must be sent back to the agent");
    };
    assert_eq!(next_attempt, 2);
    assert!(
        feedback.contains("attempt 1 failed") && feedback.contains("subtracts"),
        "{feedback}"
    );

    let (second, changeset) = compiled("loop", 2, MONEY_BASE, &with_sub("  return -(b - a);\n"));
    let attempt2 = judge(&fx, &second, next_attempt);
    assert_eq!(
        attempt2.outcome.verdict(),
        HiloValidationState::ValidatedGreen,
        "{}",
        report(&attempt2.outcome)
    );
    assert!(attempt2.first_verdict.unwrap().1 < Duration::from_secs(2));
    let kinds: Vec<_> = attempt2
        .outcome
        .steps
        .iter()
        .map(|step| step.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            StepKind::ImpactedTests,
            StepKind::Lint,
            StepKind::TypeCheck,
            StepKind::FullSuite
        ]
    );
    assert_eq!(
        correction.record(&attempt2.outcome),
        CorrectionDecision::AwaitReview
    );
    let evidence = &attempt2.outcome.evidence.evidence;
    assert_eq!(
        evidence.tested_snapshot_hash,
        second.files[0].result_content_hash
    );
    assert_eq!(evidence.checkpoint_ref, attempt2.outcome.before.reference);

    let mut documents = document(MONEY_BASE);
    let change = judged(changeset, &second, &attempt2.outcome, 2);
    let plan = accept(&fx, &mut documents, &change).unwrap();
    let session = documents.session(MONEY, 1).unwrap();
    assert_eq!(evidence.tested_snapshot_hash, session.content_blake3_hash());
    assert!(evidence.satisfies_validated_green(session.content_blake3_hash()));
    assert_eq!(plan.ranges.len(), 1);
    assert!(session
        .validation_ranges()
        .iter()
        .all(|range| range.state == HiloValidationState::ValidatedGreen));
}

/// Blocking (spec 4.5, test 1, cap): the same failure twice stops at once; distinct failures
/// stop after the second retry. No fourth attempt is ever requested.
#[test]
fn blocking_judge_loop_stops_on_a_repeated_failure_and_after_two_retries() {
    let fx = fixture();
    let regression = with_sub("  return a + b;\n");
    let mut repeated = CorrectionLoop::new();
    let (first, _) = compiled("same", 1, MONEY_BASE, &regression);
    let outcome = judge(&fx, &first, 1).outcome;
    assert!(matches!(
        repeated.record(&outcome),
        CorrectionDecision::Retry { .. }
    ));
    let (again, _) = compiled("same", 2, MONEY_BASE, &regression);
    let outcome = judge(&fx, &again, 2).outcome;
    assert_eq!(
        outcome.verdict(),
        HiloValidationState::FailedRed,
        "{}",
        report(&outcome)
    );
    assert_eq!(
        repeated.record(&outcome),
        CorrectionDecision::Stop(StopReason::SameFailureRepeated)
    );

    let attempts = [
        with_sub("  return a + b;\n"),
        MONEY_BASE.replace("  return a + b;\n", "  return a * b;\n"),
        MONEY_BASE.replace("  return a + b;\n", "  return a + b + 1;\n"),
    ];
    let mut capped = CorrectionLoop::new();
    let mut decisions = Vec::new();
    for (index, text) in attempts.iter().enumerate() {
        let attempt = capped.attempt();
        assert_eq!(attempt as usize, index + 1);
        let (change, _) = compiled("cap", attempt, MONEY_BASE, text);
        decisions.push(capped.record(&judge(&fx, &change, attempt).outcome));
    }
    assert!(matches!(
        decisions[0],
        CorrectionDecision::Retry {
            next_attempt: 2,
            ..
        }
    ));
    assert!(matches!(
        decisions[1],
        CorrectionDecision::Retry {
            next_attempt: 3,
            ..
        }
    ));
    assert_eq!(
        decisions[2],
        CorrectionDecision::Stop(StopReason::RetriesExhausted)
    );
    assert_eq!(capped.retries_used(), MAX_AUTO_RETRIES);
}

/// Blocking (spec 4.5, test 2): the user edits the function while the agent's patch is being
/// judged. The Host detects it, never overwrites the user's edit nor applies the patch at the
/// shifted offsets, and returns both texts for the merge view. An edit elsewhere is kept and
/// the patch lands on its own lines, `🟠` until verified again.
#[test]
fn blocking_concurrent_user_edit_conflicts_and_is_never_overwritten() {
    let fx = fixture();
    let proposed = with_sub("  return -(b - a);\n");
    let (compiled_change, changeset) = compiled("concurrent", 1, MONEY_BASE, &proposed);
    let outcome = judge(&fx, &compiled_change, 1).outcome;
    assert_eq!(
        outcome.verdict(),
        HiloValidationState::ValidatedGreen,
        "{}",
        report(&outcome)
    );
    let change = judged(changeset, &compiled_change, &outcome, 1);

    // While the Judge ran, the user changed the very line the patch replaces.
    let mut documents = document(MONEY_BASE);
    user_edit(
        &mut documents,
        "  return a - b;\n",
        "  return a - b + 0; // mine\n",
    );
    let before = documents.session(MONEY, 1).unwrap().text();
    match accept(&fx, &mut documents, &change) {
        Err(AcceptRefusal::Conflict {
            current_text,
            proposed_text,
            ..
        }) => {
            assert_eq!(current_text, before);
            assert_eq!(proposed_text, proposed);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    let session = documents.session(MONEY, 1).unwrap();
    assert_eq!(session.text(), before, "the user's edit is untouched");
    assert_eq!(session.interactive_revision(), 1);

    // An edit above the function moves the target: the patch follows its code.
    let mut documents = document(MONEY_BASE);
    user_edit(
        &mut documents,
        "export function add",
        "// 🦀 user note\nexport function add",
    );
    let plan = accept(&fx, &mut documents, &change).unwrap();
    let text = documents.session(MONEY, 1).unwrap().text();
    assert_eq!(text, format!("// 🦀 user note\n{proposed}"));
    assert_eq!(
        plan.ranges[0].1,
        HiloValidationState::StaleOrange,
        "rebased content was not the tested one"
    );

    // Evidence the Host did not sign is refused.
    let mut forged = change.clone();
    forged.evidence.evidence.verdict = HiloValidationState::ValidatedGreen;
    forged.evidence.evidence.tested_snapshot_hash = "f".repeat(64);
    let rope = Rope::from_str(MONEY_BASE);
    assert_eq!(
        plan_accept(&fx.authority, &forged, MONEY, &rope).unwrap_err(),
        AcceptRefusal::ForgedEvidence
    );
}

/// Blocking (spec 4.5, test 3): editing a `🟢` range makes it `🟠` synchronously; after 1.5 s
/// idle the Judge verifies the new content again: `🟢` when it passes, `🔴` when it breaks.
#[test]
fn blocking_green_range_goes_stale_on_edit_and_is_verified_again() {
    let fx = fixture();
    let (compiled_change, changeset) =
        compiled("cycle", 1, MONEY_BASE, &with_sub("  return -(b - a);\n"));
    let outcome = judge(&fx, &compiled_change, 1).outcome;
    assert_eq!(
        outcome.verdict(),
        HiloValidationState::ValidatedGreen,
        "{}",
        report(&outcome)
    );
    let change = judged(changeset, &compiled_change, &outcome, 1);
    let mut documents = document(MONEY_BASE);
    accept(&fx, &mut documents, &change).unwrap();
    let state = |documents: &DocumentService| {
        documents.session(MONEY, 1).unwrap().validation_ranges()[0].state
    };
    assert_eq!(state(&documents), HiloValidationState::ValidatedGreen);

    let mut reverifier = StaleReverifier::new();
    let verify = |documents: &mut DocumentService, run: u32| {
        let session = documents.session(MONEY, 1).unwrap();
        let text = session.text();
        let spans: Vec<_> = session
            .validation_ranges()
            .iter()
            .map(|range| range.span_utf8)
            .collect();
        let impacted = impacted_tests(&fx.analyzer, &[(MONEY, text.as_str(), spans)]);
        let files = BTreeMap::from([(MONEY.to_string(), text)]);
        let outcome = fx
            .pipeline
            .evaluate(
                &Verification {
                    checkpoint_task: &format!("reverify.{run}"),
                    task_id: "reverify",
                    files: &files,
                    buffers: &BTreeMap::new(),
                    impacted_tests: &impacted,
                },
                &mut |_| {},
            )
            .unwrap();
        let hash = session.content_blake3_hash().to_string();
        let ranges = reverified_ranges(
            session.validation_ranges(),
            &outcome.evidence.evidence,
            MONEY,
            &hash,
        )
        .unwrap();
        assert!(reverified_ranges(
            session.validation_ranges(),
            &outcome.evidence.evidence,
            MONEY,
            "other"
        )
        .is_none());
        documents
            .session_mut(MONEY, 1)
            .unwrap()
            .set_validation_ranges(ranges)
            .unwrap();
    };

    let t0 = Instant::now();
    user_edit(
        &mut documents,
        "  return -(b - a);\n",
        "  // Same result.\n  return -(b - a);\n",
    );
    assert_eq!(
        state(&documents),
        HiloValidationState::StaleOrange,
        "synchronously, on the edit"
    );
    reverifier.note_edit(MONEY, t0);
    assert!(reverifier
        .take_due(t0 + Duration::from_millis(1000))
        .is_empty());
    assert_eq!(reverifier.take_due(t0 + REVERIFY_IDLE), [MONEY]);
    verify(&mut documents, 1);
    assert_eq!(state(&documents), HiloValidationState::ValidatedGreen);

    user_edit(&mut documents, "  return -(b - a);\n", "  return b - a;\n");
    assert_eq!(state(&documents), HiloValidationState::StaleOrange);
    reverifier.note_edit(MONEY, t0);
    assert_eq!(reverifier.take_due(t0 + REVERIFY_IDLE), [MONEY]);
    verify(&mut documents, 2);
    assert_eq!(state(&documents), HiloValidationState::FailedRed);
}
