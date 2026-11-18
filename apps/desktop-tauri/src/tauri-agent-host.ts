import type { AcpEvent, AgentHost } from "@coloride/web-ui";
type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
export interface AgentChannel {
  onmessage: (event: AcpEvent) => void;
}
export function createTauriAgentHost(invoke: Invoke, channel: () => AgentChannel): AgentHost {
  return {
    async start(config, onEvent) {
      const events = channel();
      events.onmessage = onEvent;
      const id = await invoke<number>("agent_start_command", {
        installedAgentId: config.installedAgentId,
        events,
      });
      return {
        startLogin: () => invoke("agent_terminal_auth_start_command", { id }),
        writeLogin: (data) => invoke("agent_terminal_auth_input_command", { id, data }),
        resizeLogin: (cols, rows) =>
          invoke("agent_terminal_auth_resize_command", { id, cols, rows }),
        prompt: (text) => invoke("agent_prompt_command", { id, text }),
        cancel: () => invoke("agent_cancel_command", { id }),
        stop: () => invoke("agent_stop_command", { id }),
        answer: (requestId, allow) => invoke("agent_permission_command", { id, requestId, allow }),
        elicit: (requestId, action) =>
          invoke("agent_elicitation_command", { id, requestId, action }),
      };
    },
  };
}
