// @vitest-environment happy-dom
import type { EditorTransaction } from "@hilo/protocol-types";
import { describe, expect, it } from "vitest";
import {
  type BinaryIpcChannel,
  HiloEditorView,
  MergeReviewView,
  decodeEditorTransactionBinary,
} from "./index.js";

const ORIGINAL = "export function sub(a: number, b: number): number {\n  return a - b;\n}\n";
const PROPOSED = "export function sub(a: number, b: number): number {\n  return -(b - a);\n}\n";

describe("MergeReviewView (LOT-015)", () => {
  it("shows the proposed change unified or side by side, read-only either way", () => {
    const review = new MergeReviewView(document, {
      path: "src/money.ts",
      original: ORIGINAL,
      proposed: PROPOSED,
    });
    document.body.append(review.element);
    expect(review.getLayout()).toBe("unified");
    expect(review.element.dataset.layout).toBe("unified");
    expect(review.getProposedText()).toBe(PROPOSED);
    // The unified view shows the removed line next to the inserted one.
    expect(review.element.querySelector(".cm-deletedChunk")).not.toBeNull();
    expect(review.element.querySelector(".cm-changedLine")).not.toBeNull();
    const content = review.element.querySelector<HTMLElement>(".cm-content");
    expect(content?.getAttribute("contenteditable")).toBe("false");

    review.setLayout("side-by-side");
    expect(review.element.querySelector(".cm-mergeView")).not.toBeNull();
    expect(review.element.querySelectorAll(".cm-editor")).toHaveLength(2);
    expect(review.getOriginalText()).toBe(ORIGINAL);
    expect(review.getProposedText()).toBe(PROPOSED);
    for (const side of review.element.querySelectorAll<HTMLElement>(".cm-content")) {
      expect(side.getAttribute("contenteditable")).toBe("false");
    }

    review.destroy();
    expect(review.element.childElementCount).toBe(0);
  });
});

class RecordingChannel implements BinaryIpcChannel {
  readonly sent: EditorTransaction[] = [];
  openDocument(): void {}
  sendBinaryTransaction(payload: Uint8Array): void {
    this.sent.push(decodeEditorTransactionBinary(payload));
  }
  requestAtomicSave(): Promise<Uint8Array> {
    return Promise.reject(new Error("unused"));
  }
  onBinarySnapshot(): () => void {
    return () => {};
  }
}

describe("HiloEditorView.applyAcceptedChange (LOT-015)", () => {
  it("dispatches the planned edits as an agent transaction from the planned revision only", () => {
    const channel = new RecordingChannel();
    const editor = new HiloEditorView({
      initialDocumentId: "src/money.ts",
      initialContent: ORIGINAL,
      ipcChannel: channel,
    });
    const container = document.createElement("div");
    document.body.append(container);
    editor.mount(container);
    const from = ORIGINAL.indexOf("a - b");
    const change = [{ from, to: from + 5, insert: "-(b - a)" }];

    expect(editor.applyAcceptedChange("src/other.ts", 1, 0, change, "task-1")).toBe(false);
    expect(editor.applyAcceptedChange("src/money.ts", 1, 1, change, "task-1")).toBe(false);
    expect(channel.sent).toHaveLength(0);

    expect(editor.applyAcceptedChange("src/money.ts", 1, 0, change, "task-1")).toBe(true);
    expect(editor.getDocumentText()).toBe(PROPOSED);
    expect(channel.sent).toHaveLength(1);
    const tx = channel.sent[0];
    expect(tx?.origin).toEqual({ Agent: { task_id: "task-1" } });
    expect(tx?.base_revision).toBe(0);
    expect(tx?.changes).toEqual([
      { range_utf16: { start: from, end: from + 5 }, range_utf8: null, inserted_text: "-(b - a)" },
    ]);
    // The same singleton view the whole time.
    expect(editor.getStoreSnapshot().mountCount).toBe(1);
    editor.destroy();
  });
});
