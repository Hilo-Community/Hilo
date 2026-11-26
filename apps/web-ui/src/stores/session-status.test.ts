import { describe, expect, it } from "vitest";
import { createCockpitStoreBundle } from "./index.js";
import { type AgentBadgeInputs, nextAgentBadge } from "./session-status.js";
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

describe("an agent panel's badge when its session is selected", () => {
  const badge = (stores: ReturnType<typeof setup>["stores"]) => {
    const chat = stores.zone3.getState().chats.find((c) => c.chatId === "a");
    return chat?.kind === "agent" ? chat.statusBadge : undefined;
  };

  it("keeps a running turn or a pending permission", () => {
    const { stores } = setup();
    for (const current of ["running", "waiting"] as const) {
      stores.selectChat("t");
      stores.setChatStatusBadge("a", current);
      stores.selectChat("a");
      expect(badge(stores)).toBe(current);
    }
  });

  it("dismisses a finished turn or an error", () => {
    const { stores } = setup();
    for (const reported of ["done", "error"] as const) {
      stores.selectChat("t");
      stores.setChatStatusBadge("a", reported);
      stores.selectChat("a");
      expect(badge(stores)).toBeUndefined();
    }
  });
});

describe("an agent panel's badge as its store changes", () => {
  const idle: AgentBadgeInputs = { busy: false, pendingPermissions: 0, error: "" };
  const busy: AgentBadgeInputs = { ...idle, busy: true };

  it("follows a turn: running, then done when it ends", () => {
    expect(nextAgentBadge(idle, busy, undefined)).toBe("running");
    expect(nextAgentBadge(busy, busy, "running")).toBe("running");
    expect(nextAgentBadge(busy, idle, "running")).toBe("done");
  });

  it("keeps a finished turn's mark through unrelated store changes", () => {
    // Typing in the prompt box, a late message: the store changes, the agent stays idle.
    expect(nextAgentBadge(idle, idle, "done")).toBe("done");
    // Once the user has dismissed it, nothing brings it back.
    expect(nextAgentBadge(idle, idle, undefined)).toBeUndefined();
  });

  it("shows a pending permission, then the turn it belongs to", () => {
    const asking = { ...busy, pendingPermissions: 1 };
    expect(nextAgentBadge(busy, asking, "running")).toBe("waiting");
    expect(nextAgentBadge(asking, busy, "waiting")).toBe("running");
    // Answered with nothing running: nothing is waiting any more.
    expect(nextAgentBadge({ ...idle, pendingPermissions: 1 }, idle, "waiting")).toBeUndefined();
  });

  it("reports a new error, and a turn that ended on one", () => {
    expect(nextAgentBadge(idle, { ...idle, error: "boom" }, undefined)).toBe("error");
    expect(nextAgentBadge(busy, { ...idle, error: "boom" }, "running")).toBe("error");
    // The same error, already seen and dismissed, is not reported again.
    expect(nextAgentBadge({ ...idle, error: "boom" }, { ...idle, error: "boom" }, undefined)).toBe(
      undefined,
    );
  });
});
