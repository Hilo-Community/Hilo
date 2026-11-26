// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import type { CockpitTerminalSession } from "../../stores/index.js";
import { sessionIcon, sessionStatusDot } from "./session-presentation.js";

const terminal = (overrides: Partial<CockpitTerminalSession> = {}): CockpitTerminalSession => ({
  kind: "terminal",
  chatId: "t1",
  title: "Terminal",
  validationState: "UnchangedBlue",
  ...overrides,
});

describe("a terminal with a recognized agent CLI typed into it", () => {
  it("shows the plain terminal icon and no dot by default", () => {
    const chat = terminal();
    expect(sessionIcon(chat)).toContain("hilo-terminal-icon");
    expect(sessionStatusDot(chat)).toBeNull();
  });

  it("swaps to the agent's mark and a running spinner while it looks busy", () => {
    const busy = terminal({ detectedAgentId: "codex", detectedAgentBusy: true });
    expect(sessionIcon(busy)).not.toContain("hilo-terminal-icon");
    expect(sessionStatusDot(busy)).toBe("running");
  });

  it("keeps the agent's mark but no dot once it's idle at its own prompt", () => {
    const idle = terminal({ detectedAgentId: "codex", detectedAgentBusy: false });
    expect(sessionIcon(idle)).not.toContain("hilo-terminal-icon");
    expect(sessionStatusDot(idle)).toBeNull();
  });

  it("shows the done mark once its turn finished and the session hasn't been looked at", () => {
    const finished = terminal({
      detectedAgentId: "codex",
      detectedAgentBusy: false,
      detectedAgentFinished: true,
    });
    expect(sessionStatusDot(finished)).toBe("done");
  });

  it("falls back to the terminal icon for an unrecognized detected id", () => {
    const unknown = terminal({ detectedAgentId: "some-other-cli", detectedAgentBusy: true });
    expect(sessionIcon(unknown)).toContain("hilo-terminal-icon");
  });

  it("still reports an exited shell when no agent was ever detected", () => {
    expect(sessionStatusDot(terminal({ running: false }))).toBe("exited");
  });
});
