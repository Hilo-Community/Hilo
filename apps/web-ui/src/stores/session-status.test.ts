import { describe, expect, it } from "vitest";
import { createCockpitStoreBundle } from "./index.js";
import type { CockpitTerminalSession } from "./types.js";

function setup() {
  const stores = createCockpitStoreBundle({
    initialChats: [
      {
        kind: "agent",
        chatId: "a",
        title: "a",
        modelName: "Codex",
        validationState: "UnchangedBlue",
      },
      { kind: "terminal", chatId: "t", title: "Terminal", validationState: "UnchangedBlue" },
    ],
  });
  const terminal = () =>
    stores.zone3.getState().chats.find((c) => c.chatId === "t") as CockpitTerminalSession;
  return { stores, terminal };
}

describe("a detected agent's finished turn in a terminal", () => {
  it("is marked finished when the agent goes from busy to idle", () => {
    const { stores, terminal } = setup();
    stores.setTerminalAgentStatus("t", "codex", false);
    expect(terminal().detectedAgentFinished).toBe(false);
    stores.setTerminalAgentStatus("t", "codex", true);
    stores.setTerminalAgentStatus("t", "codex", false);
    expect(terminal().detectedAgentFinished).toBe(true);
  });

  it("stays marked while the agent stays idle, and clears when it works again", () => {
    const { stores, terminal } = setup();
    stores.setTerminalAgentStatus("t", "codex", true);
    stores.setTerminalAgentStatus("t", "codex", false);
    stores.setTerminalAgentStatus("t", "codex", false);
    expect(terminal().detectedAgentFinished).toBe(true);
    stores.setTerminalAgentStatus("t", "codex", true);
    expect(terminal().detectedAgentFinished).toBe(false);
  });

  it("clears when the agent exits", () => {
    const { stores, terminal } = setup();
    stores.setTerminalAgentStatus("t", "codex", true);
    stores.setTerminalAgentStatus("t", "codex", false);
    stores.setTerminalAgentStatus("t", null, false);
    expect(terminal().detectedAgentFinished).toBe(false);
  });

  it("clears once the session is selected", () => {
    const { stores, terminal } = setup();
    stores.setTerminalAgentStatus("t", "codex", true);
    stores.setTerminalAgentStatus("t", "codex", false);
    stores.selectChat("a");
    expect(terminal().detectedAgentFinished).toBe(true);
    stores.selectChat("t");
    expect(terminal().detectedAgentFinished).toBe(false);
  });
});
