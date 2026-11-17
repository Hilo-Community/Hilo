import { agentMark } from "../../settings/agent-marks.js";
import type { CockpitChatTab } from "../../stores/index.js";
import { CODEX_ICON, TERMINAL_ICON } from "./panel-icons.js";

/** Session identity is independent of a shell name or an agent's display title. */
export function sessionIcon(session: CockpitChatTab): string | null {
  if (session.kind === "terminal") return TERMINAL_ICON;
  if (session.agentId)
    return agentMark(document, session.agentId, session.agentName ?? session.title).outerHTML;
  return session.modelName === "Codex" ? CODEX_ICON : null;
}
