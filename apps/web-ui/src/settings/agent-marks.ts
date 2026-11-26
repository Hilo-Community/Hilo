import { AGENT_ICONS } from "./agent-icons.js";

/** The glyph of an agent: its brand mark when known, otherwise a tile with its first letter. */
export function agentMark(doc: Document, agentId: string, name: string): HTMLElement {
  const known = AGENT_ICONS[agentId];
  const mark = doc.createElement("span");
  mark.className = "hilo-registry-mark";
  mark.setAttribute("aria-hidden", "true");
  if (known) {
    // A constant bundled with the app (agent-icons.ts), never agent-supplied markup.
    mark.innerHTML = known;
  } else {
    mark.classList.add("hilo-registry-mark--letter");
    mark.textContent = (name || agentId).charAt(0).toUpperCase() || "?";
  }
  return mark;
}

/** A ring with a gap; the stylesheet turns it while an operation runs. */
export const SPINNER_ICON =
  '<svg class="hilo-registry-spinner" aria-hidden="true" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M12 3a9 9 0 1 0 9 9"/></svg>';

export const SEARCH_ICON =
  '<svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="10.5" cy="10.5" r="8.5"/><path d="m17 17 5 5"/></svg>';
