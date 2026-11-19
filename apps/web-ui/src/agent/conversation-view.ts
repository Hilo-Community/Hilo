import { IncrementalMarkdownCache } from "../ui/stream-renderer.js";
import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import { ExplorationView, activityEntries, reconcileElements } from "./activity-view.js";
import { ChangesView, fileChanges } from "./changes-view.js";
import { formatDuration } from "./duration.js";
import type { AgentState, Thought } from "./store.js";
import { CardView, ToolCardView } from "./tool-card-view.js";

export type ButtonFactory = (label: string, click: () => void) => HTMLButtonElement;
type Actions = {
  openLocation: (path: string, line: number) => void;
  answer: (permissionId: string, allow: boolean) => void;
};

/** How long a thought that has just finished stays open before it folds. */
const THOUGHT_LINGER_MS = 1000;

/** One cache per panel, pruned on removal/reset. Streaming preserves disclosure DOM and focus. */
export class ConversationView {
  private readonly tools = new Map<string, ToolCardView>();
  private readonly explorations = new Map<string, ExplorationView>();
  private readonly changes = new Map<string, ChangesView>();
  private note: HTMLElement | undefined;
  private turnTime: HTMLElement | undefined;
  private readonly thoughts = new Map<string, CardView>();
  /** A finished thought stays open a moment so its last words can be read, then folds. */
  private readonly lingering = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly wasRunning = new Map<string, boolean>();
  /** Each thought's Markdown, kept so a growing thought re-reads only its last block. */
  private readonly thoughtText = new Map<
    string,
    { source: string; cache: IncrementalMarkdownCache }
  >();
  private readonly permissions = new Map<string, { view: CardView; reason: string }>();
  elements(
    doc: Document,
    state: AgentState,
    button: ButtonFactory,
    actions: Actions,
  ): HTMLElement[] {
    const content: HTMLElement[] = [];
    if (state.restored && state.timeline.length) {
      if (!this.note) {
        this.note = doc.createElement("p");
        this.note.className = "coloride-agent-restored-note";
        this.note.textContent = UI_STRINGS.agent.restoredNote;
      }
      content.push(this.note);
    }
    const messages = new Map(state.messages.map((message) => [message.id, message]));
    const thoughts = new Map(state.thoughts.map((thought) => [thought.id, thought]));
    const tools = new Map(state.tools.map((tool) => [tool.id, tool]));
    // Everything is drawn in the order it happened: text, then the tool it announced, then more.
    const entries = activityEntries(state.timeline, tools);
    // The last tool waiting for an answer: its permission card is drawn right under it.
    let waiting: HTMLElement | null = null;
    // Tools of the turn in progress; a turn that edited several files ends with a summary.
    let turn: AgentState["tools"] = [];
    const usedChanges = new Set<string>();
    const endTurn = () => {
      const first = turn[0];
      const files = fileChanges(turn);
      turn = [];
      if (!first || files.length < 2) return;
      usedChanges.add(first.id);
      let view = this.changes.get(first.id);
      if (!view) {
        view = new ChangesView(doc);
        this.changes.set(first.id, view);
      }
      content.push(view.update(files, actions.openLocation));
    };
    const toolElement = (tool: AgentState["tools"][number]) => {
      turn.push(tool);
      let view = this.tools.get(tool.id);
      if (!view) {
        view = new ToolCardView(doc);
        this.tools.set(tool.id, view);
      }
      const element = view.update(tool, actions.openLocation);
      element.removeAttribute("role");
      if (tool.status === "pending") waiting = element;
      return element;
    };
    for (const ref of entries) {
      if (ref.type === "message") {
        const message = messages.get(ref.id);
        if (!message) continue;
        if (message.role === "user") endTurn();
        const article = doc.createElement("article");
        article.setAttribute("aria-label", message.role);
        article.dataset.role = message.role;
        article.innerHTML = message.html.join("");
        content.push(article);
      } else if (ref.type === "thought") {
        const thought = thoughts.get(ref.id);
        if (thought) content.push(this.thought(doc, thought));
      } else if (ref.type === "exploration") {
        let view = this.explorations.get(ref.id);
        if (!view) {
          view = new ExplorationView(doc);
          this.explorations.set(ref.id, view);
        }
        const children = ref.tools.map(toolElement);
        const element = view.update(ref.tools, children);
        if (ref.tools.some((tool) => tool.status === "pending")) waiting = element;
        content.push(element);
      } else {
        const tool = tools.get(ref.id);
        if (!tool) continue;
        content.push(toolElement(tool));
      }
    }
    if (!state.busy) endTurn();
    for (const key of this.changes.keys()) if (!usedChanges.has(key)) this.changes.delete(key);
    if (state.plan.length) content.push(planList(doc, state.plan));
    // How long the turn took, once it is over: where the time went is worth knowing.
    const timing = state.turn;
    if (timing?.endedAt !== undefined && !state.busy && state.timeline.length) {
      this.turnTime ??= doc.createElement("p");
      this.turnTime.className = "coloride-agent-turn-time";
      const waiting = timing.waitingMs >= 1000 ? formatDuration(timing.waitingMs) : undefined;
      this.turnTime.textContent = uiText.workedFor(
        formatDuration(timing.endedAt - timing.startedAt),
        waiting,
      );
      content.push(this.turnTime);
    }
    const groupIds = new Set(
      entries.filter((ref) => ref.type === "exploration").map((ref) => ref.id),
    );
    for (const key of this.explorations.keys())
      if (!groupIds.has(key)) this.explorations.delete(key);
    for (const key of this.tools.keys())
      if (!state.tools.some((tool) => tool.id === key)) this.tools.delete(key);
    for (const key of this.thoughts.keys())
      if (!state.thoughts.some((thought) => thought.id === key)) {
        this.thoughts.delete(key);
        this.thoughtText.delete(key);
        this.wasRunning.delete(key);
        clearTimeout(this.lingering.get(key));
        this.lingering.delete(key);
      }
    for (const key of this.permissions.keys()) {
      if (!state.permissions.some((permission) => permission.id === key))
        this.permissions.delete(key);
    }
    for (const permission of state.permissions) {
      let cached = this.permissions.get(permission.id);
      if (!cached || cached.reason !== permission.reason) {
        const view = new CardView(doc);
        view.element.classList.add("coloride-agent-permission");
        const reason = doc.createElement("p");
        reason.className = "coloride-agent-card-note";
        reason.textContent = permission.reason;
        const choices = doc.createElement("div");
        choices.className = "coloride-agent-permission-actions";
        choices.append(
          button(UI_STRINGS.agent.allow, () => actions.answer(permission.id, true)),
          button(UI_STRINGS.agent.reject, () => actions.answer(permission.id, false)),
        );
        view.body.append(reason, choices);
        view.update("permission", "pending", UI_STRINGS.agent.waitingPermission, "", true, true);
        cached = { view, reason: permission.reason };
        this.permissions.set(permission.id, cached);
      }
      const at = waiting ? content.indexOf(waiting) : -1;
      cached.view.element.toggleAttribute("data-attached", at >= 0);
      if (at >= 0) content.splice(at + 1, 0, cached.view.element);
      else content.push(cached.view.element);
      // A second permission goes under the first one, not between the tool and its card.
      waiting = at >= 0 ? cached.view.element : null;
    }
    return content;
  }
  private thought(doc: Document, thought: Thought): HTMLElement {
    let view = this.thoughts.get(thought.id);
    if (!view) {
      view = new CardView(doc);
      view.element.classList.add("coloride-agent-activity", "coloride-agent-thinking");
      this.thoughts.set(thought.id, view);
    }
    // A thought is written in Markdown (`**Plan the check**`), and starts with blank lines.
    const source = thought.text.trim();
    const kept = this.thoughtText.get(thought.id) ?? {
      source: "",
      cache: new IncrementalMarkdownCache(),
    };
    this.thoughtText.set(thought.id, kept);
    if (kept.source !== source || !view.body.firstChild) {
      kept.source = source;
      view.body.innerHTML = kept.cache.update(source).join("");
    }
    const label = thought.running
      ? UI_STRINGS.agent.thinking
      : thought.durationMs === undefined
        ? UI_STRINGS.agent.thought
        : thought.durationMs < 1000
          ? UI_STRINGS.agent.thoughtBriefly
          : uiText.thoughtDuration(Math.round(thought.durationMs / 1000));
    const apply = (open: boolean) =>
      view.update(
        "think",
        thought.running ? "running" : "done",
        label,
        "",
        source.length > 0,
        open,
      );
    // The reader chose by clicking, or the panel chooses: open while it thinks, fold a moment
    // after it stops. A click always wins (the card remembers it).
    if (this.wasRunning.get(thought.id) && !thought.running && !this.lingering.has(thought.id)) {
      this.lingering.set(
        thought.id,
        setTimeout(() => {
          this.lingering.delete(thought.id);
          apply(false);
        }, THOUGHT_LINGER_MS),
      );
    }
    this.wasRunning.set(thought.id, thought.running);
    apply(thought.running || this.lingering.has(thought.id));
    return view.element;
  }
  /** Stops the timers that fold finished thoughts. */
  dispose(): void {
    for (const timer of this.lingering.values()) clearTimeout(timer);
    this.lingering.clear();
  }
  render(root: HTMLElement, state: AgentState, button: ButtonFactory, actions: Actions): void {
    const children = this.elements(root.ownerDocument, state, button, actions);
    reconcileElements(root, children);
  }
}

/** Stateless rendering for isolated previews; mounted panels own a ConversationView. */
export function conversationElements(
  doc: Document,
  state: AgentState,
  button: ButtonFactory,
  actions: Actions,
): HTMLElement[] {
  return new ConversationView().elements(doc, state, button, actions);
}

export function planList(doc: Document, plan: AgentState["plan"]): HTMLElement {
  const list = doc.createElement("ol");
  list.className = "coloride-agent-plan";
  list.setAttribute("aria-label", UI_STRINGS.agent.plan);
  for (const entry of plan) {
    const item = doc.createElement("li");
    const status =
      entry.status === "completed" || entry.status === "in_progress" ? entry.status : "pending";
    item.dataset.status = status;
    if (status === "in_progress") item.setAttribute("aria-current", "step");
    const mark = doc.createElement("span");
    mark.className = "coloride-agent-plan-mark";
    mark.setAttribute("aria-hidden", "true");
    mark.textContent = status === "completed" ? "✓" : status === "in_progress" ? "◐" : "○";
    const text = doc.createElement("span");
    text.textContent = entry.content;
    const state = doc.createElement("span");
    state.className = "coloride-agent-plan-state";
    state.textContent = UI_STRINGS.agent.planStates[status];
    item.append(mark, text, state);
    list.append(item);
  }
  return list;
}
