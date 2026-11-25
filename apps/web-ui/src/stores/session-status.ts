import type { StoreContext } from "./context.js";
import type { CockpitStoreBundle } from "./types.js";

type SessionStatusActions = Pick<
  CockpitStoreBundle,
  "setChatStatusBadge" | "setTerminalRunning" | "clearChatStatusBadge" | "setTerminalAgentStatus"
>;

/**
 * Derived, at-a-glance session status for the sidebar row, header tab and grid cell icon:
 * an agent's pending permission / finished turn / error, or whether a terminal's shell is
 * still alive. Both signals already exist elsewhere (`AgentStore`, `TerminalPane`); this module
 * only mirrors them onto the shared `CockpitChatTab` so chrome that never opened the panel can
 * still render them.
 */
export function createSessionStatusActions(ctx: StoreContext): SessionStatusActions {
  const { zone3 } = ctx;
  return {
    setChatStatusBadge(
      chatId: string,
      badge: "running" | "waiting" | "done" | "error" | undefined,
    ): void {
      zone3.setState((prev) => {
        const target = prev.chats.find((c) => c.chatId === chatId);
        if (!target || target.kind !== "agent" || target.statusBadge === badge) return prev;
        return {
          ...prev,
          chats: prev.chats.map((c) =>
            c.kind === "agent" && c.chatId === chatId ? { ...c, statusBadge: badge } : c,
          ),
        };
      });
    },

    clearChatStatusBadge(chatId: string): void {
      zone3.setState((prev) => {
        const target = prev.chats.find((c) => c.chatId === chatId);
        if (!target) return prev;
        if (
          target.kind === "agent" ? target.statusBadge === undefined : !target.detectedAgentFinished
        )
          return prev;
        return {
          ...prev,
          chats: prev.chats.map((c) => {
            if (c.chatId !== chatId) return c;
            return c.kind === "agent"
              ? { ...c, statusBadge: undefined }
              : { ...c, detectedAgentFinished: false };
          }),
        };
      });
    },

    setTerminalRunning(chatId: string, running: boolean): void {
      zone3.setState((prev) => {
        const target = prev.chats.find((c) => c.chatId === chatId);
        if (!target || target.kind !== "terminal" || target.running === running) return prev;
        return {
          ...prev,
          chats: prev.chats.map((c) =>
            c.kind === "terminal" && c.chatId === chatId ? { ...c, running } : c,
          ),
        };
      });
    },

    setTerminalAgentStatus(chatId: string, agentId: string | null, busy: boolean): void {
      zone3.setState((prev) => {
        const target = prev.chats.find((c) => c.chatId === chatId);
        if (!target || target.kind !== "terminal") return prev;
        const sameAgent = (target.detectedAgentId ?? null) === agentId;
        // Set by a busy → idle turn end; kept while the same agent stays idle and unseen;
        // dropped as soon as it works again, exits, or another agent takes the terminal.
        const finished =
          agentId !== null &&
          sameAgent &&
          !busy &&
          (target.detectedAgentBusy === true || target.detectedAgentFinished === true);
        const unchanged =
          sameAgent &&
          (target.detectedAgentBusy ?? false) === busy &&
          (target.detectedAgentFinished ?? false) === finished;
        if (unchanged) return prev;
        return {
          ...prev,
          chats: prev.chats.map((c) =>
            c.kind === "terminal" && c.chatId === chatId
              ? {
                  ...c,
                  detectedAgentId: agentId ?? undefined,
                  detectedAgentBusy: busy,
                  detectedAgentFinished: finished,
                }
              : c,
          ),
        };
      });
    },
  };
}
