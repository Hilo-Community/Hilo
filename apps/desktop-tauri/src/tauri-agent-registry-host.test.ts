import { expect, it, vi } from "vitest";
import { createTauriAgentRegistryHost } from "./tauri-agent-registry-host.js";

const events = vi.hoisted(() => ({
  callbacks: new Map<string, (event: { payload: unknown }) => void>(),
  stop: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    events.callbacks.set(name, callback);
    return events.stop;
  }),
}));

it("bridges local registry changes and Settings Open requests and releases listeners", async () => {
  const invoke = vi.fn();
  const host = createTauriAgentRegistryHost(
    async <T>(command: string, args?: Record<string, unknown>) => {
      invoke(command, args);
      return undefined as T;
    },
  );
  const changed = vi.fn();
  const open = vi.fn();
  const stopChange = host.onChanged?.(changed);
  const stopOpen = host.onOpenRequested?.(open);
  await Promise.resolve();
  expect(changed).toHaveBeenCalledOnce();
  events.callbacks.get("agent-registry-changed")?.({ payload: null });
  expect(changed).toHaveBeenCalledTimes(2);
  const agent = { agentId: "codex-acp", name: "Codex" };
  events.callbacks.get("agent-open-requested")?.({ payload: agent });
  expect(open).toHaveBeenCalledWith(agent);
  await host.open?.("codex-acp");
  expect(invoke).toHaveBeenCalledWith("agent_registry_open_command", { agentId: "codex-acp" });
  stopChange?.();
  stopOpen?.();
  expect(events.stop).toHaveBeenCalledTimes(2);
  events.callbacks.get("agent-open-requested")?.({ payload: agent });
  expect(open).toHaveBeenCalledOnce();
});
