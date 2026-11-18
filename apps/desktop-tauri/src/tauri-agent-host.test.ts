import { expect, it, vi } from "vitest";
import { type AgentChannel, createTauriAgentHost } from "./tauri-agent-host.js";

it("only sends an installed id to Rust and streams events to the panel", async () => {
  const invoke = vi.fn();
  const channel: AgentChannel = { onmessage: () => {} };
  const host = createTauriAgentHost(
    async <T>(command: string, args?: Record<string, unknown>) => {
      invoke(command, args);
      return 7 as T;
    },
    () => channel,
  );
  const receive = vi.fn();
  const handle = await host.start({ installedAgentId: "codex-acp" }, receive);
  expect(invoke).toHaveBeenCalledWith("agent_start_command", {
    installedAgentId: "codex-acp",
    events: channel,
  });
  channel.onmessage({ type: "ready", sessionId: "session" });
  expect(receive).toHaveBeenCalledOnce();
  await handle.prompt("hello");
  await handle.stop();
  expect(invoke).toHaveBeenCalledWith("agent_prompt_command", { id: 7, text: "hello" });
  expect(invoke).toHaveBeenCalledWith("agent_stop_command", { id: 7 });
});
