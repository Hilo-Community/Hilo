// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import {
  InlineCompletionPanel,
  type InlineCompletionSettings,
  type InlineCompletionStatus,
} from "./inline-completion-panel.js";

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

function status(overrides: Partial<InlineCompletionStatus> = {}): InlineCompletionStatus {
  return {
    settings: { enabled: true, modelId: null },
    activeModel: "ollama/qwen2.5-coder:1.5b-base",
    ollamaRunning: true,
    models: [
      {
        id: "ollama/qwen2.5-coder:1.5b-base",
        name: "Qwen 1.5B",
        installed: true,
        fitsInMemory: true,
      },
      {
        id: "ollama/qwen2.5-coder:7b-base",
        name: "Qwen 7B",
        installed: false,
        fitsInMemory: false,
      },
    ],
    availableMemoryBytes: 4 * 1024 ** 3,
    installHint: null,
    ...overrides,
  };
}

function mount(initial: InlineCompletionStatus) {
  const saved: InlineCompletionSettings[] = [];
  let current = initial;
  const panel = new InlineCompletionPanel(document, {
    status: async () => current,
    save: async (settings) => {
      saved.push(settings);
      current = { ...current, settings };
    },
  });
  document.body.append(panel.element);
  return { panel, saved };
}

describe("LOT-014 — local AI completion settings", () => {
  it("says what is ready and saves the user's choice", async () => {
    const { panel, saved } = mount(status());
    await settle();
    expect(panel.element.textContent).toContain("Ready: Qwen 1.5B.");
    expect(panel.element.textContent).toContain("Automatic (Qwen 1.5B)");
    expect(panel.element.textContent).toContain("Qwen 7B — not installed, needs more free memory");
    const select = panel.element.querySelector("select") as HTMLSelectElement;
    select.value = "ollama/qwen2.5-coder:7b-base";
    select.dispatchEvent(new Event("change"));
    const toggle = panel.element.querySelector("input") as HTMLInputElement;
    toggle.checked = false;
    toggle.dispatchEvent(new Event("change"));
    await settle();
    expect(saved).toEqual([
      { enabled: true, modelId: "ollama/qwen2.5-coder:7b-base" },
      { enabled: false, modelId: "ollama/qwen2.5-coder:7b-base" },
    ]);
    panel.destroy();
  });

  it("tells how to get the runtime or the model when they are missing", async () => {
    const stopped = mount(status({ ollamaRunning: false }));
    await settle();
    expect(stopped.panel.element.textContent).toContain("Ollama is not running");
    stopped.panel.destroy();
    const missing = mount(status({ installHint: "ollama pull qwen2.5-coder:1.5b-base" }));
    await settle();
    expect(missing.panel.element.querySelector("code")?.textContent).toBe(
      "ollama pull qwen2.5-coder:1.5b-base",
    );
    missing.panel.destroy();
  });
});
