// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentRegistryHost, InstalledAgent } from "../../settings/agent-registry.js";
import { NewPanelMenu } from "./new-panel-menu.js";

const installed: InstalledAgent = {
  id: "codex@1",
  agentId: "codex",
  name: "Codex",
  version: "1.0",
  distribution: "npm",
};
const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};
afterEach(() => document.body.replaceChildren());

describe("installed agents in the new panel menu", () => {
  function setup(initial: InstalledAgent[]) {
    let agents = initial;
    let changed = () => {};
    const stop = vi.fn();
    const host: AgentRegistryHost = {
      list: vi.fn(async () => ({
        agents: [],
        installed: agents,
        origin: "saved" as const,
        refreshError: null,
      })),
      install: vi.fn(),
      uninstall: vi.fn(),
      onChanged: (listener) => {
        changed = listener;
        return stop;
      },
    };
    const menu = new NewPanelMenu(document, host);
    const anchor = document.createElement("button");
    document.body.append(anchor);
    const openAgentInterface = vi.fn();
    const installAgents = vi.fn();
    const show = () => {
      menu.toggle(anchor, { openAgentInterface, installAgents });
      document.querySelector<HTMLButtonElement>(".has-submenu")?.click();
    };
    return {
      menu,
      host,
      stop,
      show,
      openAgentInterface,
      installAgents,
      change: (next: InstalledAgent[]) => {
        agents = next;
        changed();
      },
    };
  }

  it("lists local installations with their mark, name and version", async () => {
    const s = setup([installed]);
    await settle();
    s.show();
    const entries = document.querySelectorAll<HTMLButtonElement>(
      ".coloride-new-panel-submenu button",
    );
    expect(entries).toHaveLength(2);
    expect(entries[0]?.textContent).toContain("Codex1.0");
    expect(entries[0]?.querySelector(".coloride-registry-mark")).not.toBeNull();
    expect(entries[1]?.textContent).toBe("Install more agents…");
    entries[0]?.click();
    expect(s.openAgentInterface).toHaveBeenCalledWith(installed);
    expect(s.host.list).toHaveBeenCalledWith(false);
    s.menu.destroy();
    expect(s.stop).toHaveBeenCalledOnce();
  });

  it("offers installation with no agents and refreshes an open submenu after changes", async () => {
    const s = setup([]);
    await settle();
    s.show();
    const entries = () => [
      ...document.querySelectorAll<HTMLButtonElement>(".coloride-new-panel-submenu button"),
    ];
    expect(entries().map((b) => b.textContent)).toEqual(["Install an agent…"]);
    s.change([installed]);
    await settle();
    expect(entries()).toHaveLength(2);
    s.change([]);
    await settle();
    entries()[0]?.click();
    expect(s.installAgents).toHaveBeenCalledOnce();
    expect(s.host.list).toHaveBeenCalledTimes(3);
    s.menu.destroy();
  });
});
