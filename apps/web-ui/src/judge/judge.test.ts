// @vitest-environment happy-dom
import {
  HiloEditorView,
  InMemoryBinaryHostBridge,
  decodeEditorTransactionBinary,
} from "@hilo/editor-cm6";
import type { EditorTransaction } from "@hilo/protocol-types";
import { describe, expect, it, vi } from "vitest";
import { CockpitDomController, createCockpitStoreBundle } from "../index.js";
import { JudgeController } from "./controller.js";
import { JudgeReviewPanel, type ReviewPanelActions } from "./review-panel.js";
import type { AcceptResponse, JudgeEvent, JudgeHost, JudgeReview } from "./types.js";

const BASE = "export function sub(a: number, b: number): number {\n  return a - b;\n}\n";
const PROPOSED = "export function sub(a: number, b: number): number {\n  return -(b - a);\n}\n";

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};

function review(overrides: Partial<JudgeReview> = {}): JudgeReview {
  return {
    reviewId: "review-1",
    agentId: 1,
    verdict: "ValidatedGreen",
    attempts: 2,
    status: "Pending",
    stopped: null,
    files: [{ path: "src/money.ts", base_text: BASE, proposed_text: PROPOSED }],
    steps: [
      { kind: "ImpactedTests", passed: true, exitCode: 0, durationMs: 412 },
      { kind: "Lint", passed: true, exitCode: 0, durationMs: 30 },
    ],
    diagnostics: [],
    preview: null,
    ...overrides,
  };
}

const button = (root: HTMLElement, text: string) =>
  [...root.querySelectorAll("button")].find((b) => b.textContent === text) as HTMLButtonElement;

function actions(accept: () => Promise<AcceptResponse>): ReviewPanelActions & {
  calls: string[];
} {
  const calls: string[] = [];
  return {
    calls,
    accept: async (path) => {
      calls.push(`accept:${path}`);
      return accept();
    },
    reject: async () => {
      calls.push("reject");
    },
    adjust: async (note) => {
      calls.push(`adjust:${note}`);
    },
    close: () => calls.push("close"),
  };
}

describe("JudgeReviewPanel (LOT-015)", () => {
  it("shows the state, the evidence as text and the diff, and sends each decision", async () => {
    const hostile = '<img src=x onerror="alert(1)">';
    const recorded = actions(async () => ({
      type: "apply",
      taskId: "t",
      baseRevision: 0,
      changes: [],
      state: "ValidatedGreen",
    }));
    const panel = new JudgeReviewPanel(
      document,
      review({
        verdict: "FailedRed",
        stopped: "SameFailureRepeated",
        steps: [{ kind: "ImpactedTests", passed: false, exitCode: 1, durationMs: 400 }],
        diagnostics: [
          {
            filePath: "src/money.test.ts",
            line: 1,
            column: 1,
            source: "TestRunner",
            code: "test-failed",
            message: `subtracts: expected 7 to be 3 ${hostile}`,
          },
        ],
      }),
      recorded,
    );
    document.body.append(panel.element);
    const root = panel.element;
    expect(root.querySelector(".hilo-judge-badge")?.textContent).toBe("✕ Failed");
    expect(root.querySelector(".hilo-judge-stopped")?.textContent).toContain("same failure");
    expect(root.querySelector(".hilo-judge-step")?.getAttribute("data-passed")).toBe("false");
    const diagnostic = root.querySelector(".hilo-judge-diagnostic");
    expect(diagnostic?.textContent).toContain(hostile);
    expect(root.querySelector("img")).toBeNull();
    expect(panel.getMergeView().getLayout()).toBe("unified");
    expect(panel.getMergeView().getProposedText()).toBe(PROPOSED);

    button(root, "Side by side").click();
    expect(panel.getMergeView().getLayout()).toBe("side-by-side");
    expect(root.querySelectorAll(".cm-editor")).toHaveLength(2);
    expect(button(root, "Side by side").getAttribute("aria-pressed")).toBe("true");

    button(root, "Request Adjustment").click();
    const input = root.querySelector("textarea") as HTMLTextAreaElement;
    input.value = "Keep the original formula";
    root.querySelector("form")?.dispatchEvent(new Event("submit", { cancelable: true }));
    await settle();
    button(root, "✕ Reject").click();
    await settle();
    button(root, "✓ Accept").click();
    await settle();
    expect(recorded.calls).toEqual([
      "adjust:Keep the original formula",
      "reject",
      "accept:src/money.ts",
    ]);
    expect(button(root, "✓ Accept").disabled).toBe(true);
    expect(root.querySelector(".hilo-judge-file")?.textContent).toBe("✓ src/money.ts");
    panel.destroy();
  });

  it("shows the rendered components in a frame that runs no script and has no origin", () => {
    const document_ =
      '<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="default-src \'none\'"></head><body><output aria-label="Balance">42</output><script>parent.stolen = 1</script></body></html>';
    const panel = new JudgeReviewPanel(
      document,
      review({ preview: { document: document_, a11y: 'body\n  output "Balance": 42' } }),
      actions(async () => ({
        type: "apply",
        taskId: "t",
        baseRevision: 0,
        changes: [],
        state: "ValidatedGreen",
      })),
    );
    document.body.append(panel.element);
    const frame = panel.element.querySelector("iframe");
    expect(frame?.getAttribute("sandbox")).toBe("");
    expect(frame?.getAttribute("referrerpolicy")).toBe("no-referrer");
    expect(frame?.srcdoc).toBe(document_);
    expect(frame?.src).toBe("");
    expect(Reflect.get(window, "stolen")).toBeUndefined();
    expect(panel.element.querySelector(".hilo-judge-a11y-tree")?.textContent).toContain(
      'output "Balance": 42',
    );
    // No preview, no frame.
    const plain = new JudgeReviewPanel(
      document,
      review(),
      actions(async () => ({
        type: "apply",
        taskId: "t",
        baseRevision: 0,
        changes: [],
        state: "ValidatedGreen",
      })),
    );
    expect(plain.element.querySelector("iframe")).toBeNull();
    panel.destroy();
    plain.destroy();
  });

  it("shows a conflict between the user's text and the verified change, and cannot accept it", async () => {
    const current = BASE.replace("a - b;", "a - b + 0; // mine");
    const panel = new JudgeReviewPanel(
      document,
      review(),
      actions(async () => ({
        type: "conflict",
        path: "src/money.ts",
        reason: "changed",
        baseText: BASE,
        currentText: current,
        proposedText: PROPOSED,
      })),
    );
    document.body.append(panel.element);
    button(panel.element, "✓ Accept").click();
    await settle();
    const notice = panel.element.querySelector<HTMLElement>(".hilo-judge-conflict");
    expect(notice?.hidden).toBe(false);
    expect(notice?.getAttribute("role")).toBe("alert");
    expect(panel.getMergeView().getOriginalText()).toBe(current);
    expect(panel.getMergeView().getProposedText()).toBe(PROPOSED);
    expect(button(panel.element, "✓ Accept").disabled).toBe(true);
    expect(button(panel.element, "✕ Reject").disabled).toBe(false);
    panel.destroy();
  });
});

/** A Judge host whose events the test sends. */
function fakeHost(accept: (documentId: string, generation: number) => AcceptResponse) {
  let listener: ((event: JudgeEvent) => void) | null = null;
  const host: JudgeHost & { accepted: Array<[string, string, string, number]> } = {
    accepted: [],
    listen: async (next) => {
      listener = next;
      return () => {
        listener = null;
      };
    },
    reviews: async () => [],
    accept: async (reviewId, path, documentId, generation) => {
      host.accepted.push([reviewId, path, documentId, generation]);
      return accept(documentId, generation);
    },
    reject: vi.fn(async () => {}),
    adjust: vi.fn(async () => {}),
    documentStates: async () => [],
  };
  return { host, emit: (event: JudgeEvent) => listener?.(event) };
}

class RecordingBridge extends InMemoryBinaryHostBridge {
  readonly sent: EditorTransaction[] = [];
  override sendBinaryTransaction(payload: Uint8Array): void {
    this.sent.push(decodeEditorTransactionBinary(payload));
    super.sendBinaryTransaction(payload);
  }
}

describe("JudgeController (LOT-015)", () => {
  it("accepts through the singleton editor as an agent edit and draws the Host's states", async () => {
    const bridge = new RecordingBridge();
    const editor = new HiloEditorView({
      initialDocumentId: "src/money.ts",
      initialContent: BASE,
      ipcChannel: bridge,
    });
    const container = document.createElement("div");
    document.body.append(container);
    editor.mount(container);
    const from = BASE.indexOf("a - b");
    const { host, emit } = fakeHost(() => ({
      type: "apply",
      taskId: "task-1",
      baseRevision: 0,
      changes: [{ from, to: from + 5, insert: "-(b - a)" }],
      state: "ValidatedGreen",
    }));
    const banner = document.createElement("div");
    const shown: HTMLElement[] = [];
    let closed = 0;
    const opened: string[] = [];
    const controller = new JudgeController(document, {
      host,
      editorView: editor,
      banner,
      showReview: (element) => shown.push(element),
      closeReview: () => {
        closed += 1;
      },
      openFile: async (path) => {
        opened.push(path);
      },
    });
    await controller.start();

    emit({ type: "verifying", agentId: 1, attempt: 1 });
    expect(banner.hidden).toBe(false);
    expect(banner.textContent).toContain("verifying attempt 1");
    emit({ type: "firstVerdict", agentId: 1, attempt: 1, state: "FailedRed", elapsedMs: 412 });
    expect(banner.textContent).toContain("✕ Failed on impacted tests (0.4 s)");
    emit({ type: "reviewReady", review: review() });
    expect(shown).toHaveLength(1);
    expect(banner.hidden).toBe(true);

    const panel = controller.getPanel();
    expect(panel).not.toBeNull();
    button(panel?.element as HTMLElement, "✓ Accept").click();
    await settle();
    expect(opened).toEqual(["src/money.ts"]);
    expect(host.accepted).toEqual([["review-1", "src/money.ts", "src/money.ts", 1]]);
    expect(editor.getDocumentText()).toBe(PROPOSED);
    expect(bridge.sent.at(-1)?.origin).toEqual({ Agent: { task_id: "task-1" } });

    emit({
      type: "states",
      documentId: "src/money.ts",
      sessionGeneration: 1,
      contentHash: "h",
      decorations: [{ fromLine: 1, toLine: 3, state: "ValidatedGreen", snapshotHash: "h" }],
    });
    expect(editor.getDecorations()).toEqual([
      { fromLine: 1, toLine: 3, state: "ValidatedGreen", snapshotHash: "h" },
    ]);
    // Another document's states are not drawn on this one.
    emit({
      type: "states",
      documentId: "src/other.ts",
      sessionGeneration: 1,
      contentHash: "x",
      decorations: [{ fromLine: 1, toLine: 1, state: "FailedRed", snapshotHash: "x" }],
    });
    expect(editor.getDecorations()[0]?.state).toBe("ValidatedGreen");

    emit({ type: "reviewClosed", reviewId: "review-1", status: "Accepted" });
    expect(controller.getPanel()).toBeNull();
    expect(closed).toBe(1);
    expect(editor.getStoreSnapshot().mountCount).toBe(1);
    controller.dispose();
    editor.destroy();
  });
});

describe("Judge review in the mounted cockpit (LOT-015)", () => {
  it("replaces the editor in Zone 4 without unmounting it, and gives it back", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const stores = createCockpitStoreBundle();
    const editor = new HiloEditorView({
      initialDocumentId: "src/file.ts",
      initialContent: "const a = 1;",
      ipcChannel: new InMemoryBinaryHostBridge(),
    });
    const { host, emit } = fakeHost(() => ({
      type: "apply",
      taskId: "t",
      baseRevision: 0,
      changes: [],
      state: "ValidatedGreen",
    }));
    const cockpit = new CockpitDomController({
      rootContainer: root,
      stores,
      editorView: editor,
      judgeHost: host,
    });
    await settle();
    const instance = editor.getEditorView();
    const parent = instance.dom.parentElement as HTMLElement;
    emit({ type: "reviewReady", review: review() });
    const reviewHost = root.querySelector<HTMLElement>(".hilo-zone-4-review-host");
    expect(reviewHost?.hidden).toBe(false);
    expect(reviewHost?.querySelector(".hilo-judge-review")).not.toBeNull();
    expect(parent.style.display).toBe("none");
    expect(instance.dom.parentElement).toBe(parent);

    const close = root.querySelector<HTMLButtonElement>(".hilo-judge-close");
    close?.click();
    expect(reviewHost?.hidden).toBe(true);
    expect(parent.style.display).toBe("block");
    expect(editor.getEditorView()).toBe(instance);
    expect(instance.dom.parentElement).toBe(parent);
    expect(editor.getStoreSnapshot().mountCount).toBe(1);
    cockpit.destroy();
    editor.destroy();
    root.remove();
  });
});
