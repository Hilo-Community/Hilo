import type { TerminalAgentStatus, TerminalHost } from "@coloride/web-ui";

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** The part of Tauri's `Channel` used here. */
export interface RawChannel {
  onmessage: (message: ArrayBuffer) => void;
}

/** A Tauri `Channel` carrying parsed JSON agent-status messages. */
export interface AgentStatusChannel {
  onmessage: (message: { agentId: string | null; busy: boolean }) => void;
}

/**
 * Terminals backed by the Rust side (`commands/terminal.rs`). Output arrives as raw frames on
 * a Tauri `Channel`, at most one every 16 ms; an empty frame means the shell exited. A second,
 * near-silent channel reports a recognized agent CLI entering/leaving the foreground.
 */
export function createTauriTerminalHost(
  invoke: Invoke,
  createChannel: () => RawChannel,
  createAgentStatusChannel: () => AgentStatusChannel,
): TerminalHost {
  return {
    async open(size, onOutput, onExit, onAgentStatus) {
      const channel = createChannel();
      let exited = false;
      channel.onmessage = (message) => {
        const bytes = new Uint8Array(message);
        if (bytes.byteLength > 0) {
          onOutput(bytes);
        } else if (!exited) {
          exited = true;
          onExit();
        }
      };
      const agentStatus = createAgentStatusChannel();
      agentStatus.onmessage = (message) => {
        onAgentStatus?.({
          agentId: message.agentId,
          busy: message.busy,
        } satisfies TerminalAgentStatus);
      };
      const id = await invoke<number>("terminal_open_command", {
        cols: size.cols,
        rows: size.rows,
        output: channel,
        agentStatus,
      });
      return {
        write: (data) => void invoke("terminal_write_command", { id, data }).catch(() => undefined),
        resize: (cols, rows) =>
          void invoke("terminal_resize_command", { id, cols, rows }).catch(() => undefined),
        close: () => void invoke("terminal_close_command", { id }).catch(() => undefined),
      };
    },
  };
}
