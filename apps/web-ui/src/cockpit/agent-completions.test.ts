// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import type { CockpitChatTab } from "../stores/index.js";
import { createCockpitStoreBundle } from "../stores/index.js";
import { finishedAgents, watchAgentCompletions } from "./agent-completions.js";

const agent = (chatId: string, statusBadge?: "running" | "done" | "waiting"): CockpitChatTab => ({
  kind: "agent",
  chatId,
  title: `Agent ${chatId}`,
  modelName: "Codex",
  validationState: "UnchangedBlue",
  ...(statusBadge ? { statusBadge } : {}),
});
const terminal = (chatId: string, detectedAgentId?: string, busy?: boolean): CockpitChatTab => ({
  kind: "terminal",
  chatId,
  title: `Terminal ${chatId}`,
  validationState: "UnchangedBlue",
  ...(detectedAgentId ? { detectedAgentId, detectedAgentBusy: busy === true } : {}),
});

describe("finishedAgents", () => {
  it("reports an agent panel whose turn just ended, once", () => {
    expect(finishedAgents([agent("a", "running")], [agent("a", "done")])).toEqual([
      { chatId: "a", title: "Agent a" },
    ]);
    expect(finishedAgents([agent("a", "done")], [agent("a", "done")])).toEqual([]);
    expect(finishedAgents([agent("a", "running")], [agent("a", "waiting")])).toEqual([]);
  });

  it("reports a terminal whose agent CLI went from busy to idle, not other changes", () => {
    expect(finishedAgents([terminal("t", "codex", true)], [terminal("t", "codex", false)])).toEqual(
      [{ chatId: "t", title: "Terminal t" }],
    );
    // It left the foreground while busy (quit or crashed): not a finished turn.
    expect(finishedAgents([terminal("t", "codex", true)], [terminal("t")])).toEqual([]);
    // Idle to busy, or an ordinary shell: nothing to report.
    expect(finishedAgents([terminal("t", "codex", false)], [terminal("t", "codex", true)])).toEqual(
      [],
    );
    expect(finishedAgents([terminal("t")], [terminal("t")])).toEqual([]);
  });

  it("ignores a session that has just appeared", () => {
    expect(finishedAgents([], [agent("a", "done")])).toEqual([]);
  });
});

describe("watchAgentCompletions", () => {
  it("reports every finished turn, saying whether the user was looking at that session", () => {
    const stores = createCockpitStoreBundle({ initialChats: [agent("a"), agent("b")] });
    const notify = vi.fn();
    const attended = new Set(["a"]);
    const stop = watchAgentCompletions(stores, notify, (chatId) => attended.has(chatId));

    stores.setChatStatusBadge("a", "running");
    stores.setChatStatusBadge("b", "running");
    stores.setChatStatusBadge("a", "done");
    expect(notify).toHaveBeenLastCalledWith({ chatId: "a", title: "Agent a", attended: true });
    stores.setChatStatusBadge("b", "done");
    expect(notify).toHaveBeenLastCalledWith({ chatId: "b", title: "Agent b", attended: false });
    expect(notify).toHaveBeenCalledTimes(2);

    stop();
    stores.setChatStatusBadge("b", "running");
    stores.setChatStatusBadge("b", "done");
    expect(notify).toHaveBeenCalledTimes(2);
  });

  it("reports a terminal's agent CLI finishing", () => {
    const stores = createCockpitStoreBundle({ initialChats: [terminal("t")] });
    const notify = vi.fn();
    watchAgentCompletions(stores, notify, () => false);

    stores.setTerminalAgentStatus("t", "codex", true);
    stores.setTerminalAgentStatus("t", "codex", false);
    expect(notify).toHaveBeenCalledExactlyOnceWith({
      chatId: "t",
      title: "Terminal t",
      attended: false,
    });
  });
});
