// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import { AgentPanel } from "./panel.js";
import { type AgentHost, AgentStore } from "./store.js";

const host: AgentHost = {
  start: vi.fn(async () => ({
    prompt: vi.fn(),
    cancel: vi.fn(),
    stop: vi.fn(async () => undefined),
    answer: vi.fn(),
  })),
};
const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

describe("files the agent mentions", () => {
  it("open in the editor at their line, and nothing else is opened", async () => {
    const open = vi.fn(async () => undefined);
    const store = new AgentStore((callback) => callback());
    const panel = new AgentPanel(
      document,
      host,
      open,
      { agentId: "codex", name: "Codex" },
      false,
      store,
    );
    document.body.append(panel.element);
    store.enqueue({
      type: "update",
      update: {
        sessionUpdate: "agent_message_chunk",
        content: {
          type: "text",
          text: "See [App.jsx](<src/App.jsx:6>) and [docs](https://example.com).",
        },
      },
    });
    const ref = panel.element.querySelector<HTMLElement>(".hilo-agent-ref");
    expect(ref?.textContent).toBe("App.jsx");
    // It is text inside the paragraph, not a box of its own: same line, same flow.
    expect(ref?.tagName).toBe("SPAN");
    expect(ref?.parentElement?.tagName).toBe("P");
    expect(ref?.getAttribute("role")).toBe("link");
    ref?.click();
    await settle();
    expect(open).toHaveBeenCalledWith("src/App.jsx", 6);
    // The keyboard opens it too.
    ref?.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await settle();
    expect(open).toHaveBeenCalledTimes(2);
    // A web address is only shown: clicking it opens nothing.
    panel.element.querySelector<HTMLElement>(".hilo-agent-link")?.click();
    // The panel checks again what it is about to open.
    for (const path of ["/etc/passwd", "../secret", "C:\\x"]) {
      const forged = document.createElement("button");
      forged.className = "hilo-agent-ref";
      forged.dataset.path = path;
      forged.dataset.line = "1";
      panel.element.querySelector(".hilo-agent-output")?.append(forged);
      forged.click();
    }
    const bad = document.createElement("button");
    bad.className = "hilo-agent-ref";
    bad.dataset.path = "src/a.ts";
    bad.dataset.line = "-3";
    panel.element.querySelector(".hilo-agent-output")?.append(bad);
    bad.click();
    await settle();
    expect(open).toHaveBeenCalledTimes(2);
    panel.destroy();
  });
});
