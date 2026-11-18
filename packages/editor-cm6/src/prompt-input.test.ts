// @vitest-environment happy-dom
import { EditorView } from "@codemirror/view";
import { afterEach, describe, expect, it, vi } from "vitest";

import { createPromptInput } from "./prompt-input.js";

function input(onMultilineChange: (multiline: boolean) => void) {
  return createPromptInput(document.body, {
    placeholder: "Ask the agent…",
    ariaLabel: "Prompt",
    maxLength: 1000,
    maxHeight: 200,
    onSubmit: () => undefined,
    onMultilineChange,
  });
}

describe("prompt input", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    document.body.replaceChildren();
  });

  it("goes back to one line when emptied, even in a panel too narrow for the hint", () => {
    // A narrow panel: whatever is shown measures two lines.
    vi.spyOn(EditorView.prototype, "contentHeight", "get").mockReturnValue(42);
    vi.spyOn(EditorView.prototype, "defaultLineHeight", "get").mockReturnValue(21);
    const changes: boolean[] = [];
    const prompt = input((multiline) => changes.push(multiline));
    prompt.setText("a long question that wraps");
    expect(changes).toEqual([true]);
    prompt.setText("");
    expect(changes).toEqual([true, false]);
    prompt.destroy();
  });

  it("is multiline for a newline and one line again once the text is cleared", () => {
    const changes: boolean[] = [];
    const prompt = input((multiline) => changes.push(multiline));
    prompt.setText("first\nsecond");
    prompt.setText("");
    expect(changes).toEqual([true, false]);
    prompt.destroy();
  });
});
