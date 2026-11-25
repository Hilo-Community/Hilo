import { agentMark } from "../../settings/agent-marks.js";
import type { CockpitChatTab } from "../../stores/index.js";
import { CODEX_ICON, TERMINAL_ICON } from "./panel-icons.js";

/**
 * The agent-registry id (as `AGENT_ICONS` in `settings/agent-icons.ts` keys it) for an executable
 * name the Rust side detects running inside a plain terminal. Extend alongside
 * `host-core`'s `known_agent_binary`.
 */
const DETECTED_AGENT_REGISTRY_ID: Readonly<Record<string, string>> = {
  codex: "codex-acp",
  claude: "claude-acp",
};

/** Session identity is independent of a shell name or an agent's display title. */
export function sessionIcon(session: CockpitChatTab): string | null {
  if (session.kind === "terminal") {
    const registryId = session.detectedAgentId
      ? DETECTED_AGENT_REGISTRY_ID[session.detectedAgentId]
      : undefined;
    return registryId ? agentMark(document, registryId, registryId).outerHTML : TERMINAL_ICON;
  }
  if (session.agentId)
    return agentMark(document, session.agentId, session.agentName ?? session.title).outerHTML;
  return session.modelName === "Codex" ? CODEX_ICON : null;
}

/**
 * The small status mark shown at the icon's corner: a spinner while an agent's turn is running
 * (an `AgentStore` turn, or a detected agent CLI that looks busy), a pending permission, a
 * just-finished turn, an error, or a terminal's exited shell. `null` means nothing to show.
 */
export function sessionStatusDot(
  session: CockpitChatTab,
): "running" | "waiting" | "done" | "error" | "exited" | null {
  if (session.kind === "terminal") {
    if (session.detectedAgentId) {
      if (session.detectedAgentBusy) return "running";
      return session.detectedAgentFinished ? "done" : null;
    }
    return session.running === false ? "exited" : null;
  }
  return session.statusBadge ?? null;
}
