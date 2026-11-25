import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import type { TimelineRef, ToolCard } from "./store.js";
import { CardView } from "./tool-card-view.js";

export type ActivityEntry = TimelineRef | { type: "exploration"; id: string; tools: ToolCard[] };

/** Group only explicit ACP exploration kinds. Commands and intervening messages stay in place. */
export function activityEntries(
  timeline: TimelineRef[],
  tools: Map<string, ToolCard>,
): ActivityEntry[] {
  const entries: ActivityEntry[] = [];
  for (const ref of timeline) {
    const tool = ref.type === "tool" ? tools.get(ref.id) : undefined;
    // Keep the internal review in session state, outside the visible conversation.
    if (tool?.title.trim().toLowerCase() === "guardian review") continue;
    if (tool && ["read", "search", "fetch"].includes(tool.kind) && !tool.edit) {
      const last = entries.at(-1);
      if (last?.type === "exploration") last.tools.push(tool);
      else entries.push({ type: "exploration", id: tool.id, tools: [tool] });
    } else entries.push(ref);
  }
  return entries;
}

/** Reuse existing nodes so streaming does not reset disclosure focus or scroll. */
export function reconcileElements(root: HTMLElement, children: HTMLElement[]): void {
  const wanted = new Set(children);
  for (const child of Array.from(root.children)) {
    if (!wanted.has(child as HTMLElement)) child.remove();
  }
  let next = root.firstChild;
  for (const child of children) {
    if (child === next) next = next.nextSibling;
    else root.insertBefore(child, next);
  }
  while (next) {
    const following = next.nextSibling;
    root.removeChild(next);
    next = following;
  }
}

export class ExplorationView {
  readonly card: CardView;
  constructor(doc: Document) {
    this.card = new CardView(doc);
    this.card.element.classList.add("hilo-agent-activity", "hilo-agent-exploration");
    this.card.body.setAttribute("role", "list");
  }
  update(tools: ToolCard[], children: HTMLElement[]): HTMLElement {
    const status = tools.some((tool) => tool.status === "running")
      ? "running"
      : tools.some((tool) => tool.status === "pending")
        ? "pending"
        : tools.some((tool) => tool.status === "failed")
          ? "failed"
          : tools.some((tool) => tool.status === "denied")
            ? "denied"
            : "done";
    const label = {
      running: UI_STRINGS.agent.exploring,
      pending: UI_STRINGS.agent.explorePending,
      failed: UI_STRINGS.agent.exploreFailed,
      denied: UI_STRINGS.agent.exploreStopped,
      done: UI_STRINGS.agent.explored,
    }[status];
    const count = (kind: string) => tools.filter((tool) => tool.kind === kind).length;
    const summary = uiText.explorationSummary(count("read"), count("search"), count("fetch"));
    for (const child of children) child.setAttribute("role", "listitem");
    reconcileElements(this.card.body, children);
    this.card.update(
      "search",
      status,
      label,
      summary,
      children.length > 0,
      status === "running" || status === "pending" || status === "failed" || status === "denied",
    );
    return this.card.element;
  }
}
