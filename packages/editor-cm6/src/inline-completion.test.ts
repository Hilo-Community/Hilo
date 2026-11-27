// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  HiloEditorView,
  InMemoryBinaryHostBridge,
  type InlineCompletionRequest,
  type InlineCompletionResponse,
} from "./index.js";

function setup(text = "function add(a, b) {\n  return \n}\n") {
  const editor = new HiloEditorView({
    initialDocumentId: "src/add.ts",
    initialContent: text,
    ipcChannel: new InMemoryBinaryHostBridge(),
  });
  editor.mount(document.createElement("div"));
  const pending: Array<{
    request: InlineCompletionRequest;
    answer: (text: string | null) => void;
  }> = [];
  editor.setInlineCompletionProvider(
    (request) =>
      new Promise<InlineCompletionResponse | null>((resolve) => {
        pending.push({
          request,
          answer: (suggestion) =>
            resolve(
              suggestion === null
                ? null
                : {
                    document_id: request.document_id,
                    session_generation: request.session_generation,
                    base_revision: request.revision,
                    position_utf16: request.position_utf16,
                    text: suggestion,
                    model_id: "ollama/qwen2.5-coder:1.5b-base",
                    latency_ms: 80,
                  },
            ),
        });
      }),
  );
  const view = editor.getEditorView();
  const type = (insert: string) => {
    const pos = view.state.selection.main.head;
    view.dispatch({
      changes: { from: pos, insert },
      selection: { anchor: pos + insert.length },
      userEvent: "input.type",
    });
  };
  const client = editor.getInlineCompletion();
  return { editor, view, type, pending, client };
}

const settle = async () => {
  await vi.advanceTimersByTimeAsync(200);
};

describe("LOT-014 — inline completion as ghost text", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("asks once after a pause in typing and Tab accepts the suggestion as a normal edit", async () => {
    const { view, type, pending, client } = setup();
    view.dispatch({ selection: { anchor: 30 } });
    type("a");
    type(" +");
    await settle();
    expect(pending).toHaveLength(1);
    expect(pending[0]?.request.position_utf16).toBe(view.state.selection.main.head);
    pending[0]?.answer(" b;");
    await settle();
    expect(client.shown(view)).toBe(" b;");
    // The ghost text is not part of the document until accepted.
    expect(view.state.doc.toString()).toContain("return a +\n");
    expect(client.accept(view)).toBe(true);
    expect(view.state.doc.toString()).toContain("return a + b;\n");
    expect(view.state.selection.main.head).toBe(30 + " a +".length - 1 + " b;".length);
    expect(client.shown(view)).toBeNull();
  });

  it("an answer for a text that moved on is dropped", async () => {
    const { view, type, pending, client } = setup();
    view.dispatch({ selection: { anchor: 30 } });
    type("a");
    await settle();
    type(" ");
    pending[0]?.answer(" + b;");
    await settle();
    expect(client.shown(view)).toBeNull();
    expect(client.discardedAnswers()).toBe(1);
  });

  it("moving the cursor or Escape clears the suggestion; no request mid-word", async () => {
    const { view, type, pending, client } = setup();
    view.dispatch({ selection: { anchor: 30 } });
    type("a");
    await settle();
    pending[0]?.answer(" + b;");
    await settle();
    expect(client.shown(view)).toBe(" + b;");
    expect(client.dismiss(view)).toBe(true);
    expect(client.shown(view)).toBeNull();

    // In the middle of `function`: nothing is asked.
    view.dispatch({ selection: { anchor: 3 } });
    type("x");
    await settle();
    expect(pending).toHaveLength(1);
  });

  it("without a suggestion Tab keeps indenting", async () => {
    const { view, client } = setup();
    expect(client.shown(view)).toBeNull();
    expect(client.accept(view)).toBe(false);
  });
});
