import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import { diffLines, formatDiff } from "./diff.js";
import type { AgentState, ToolCard } from "./store.js";

/** Makes a panel button: the panel owns their look and error handling. */
export type ButtonFactory = (label: string, click: () => void) => HTMLButtonElement;

/**
 * The conversation of an agent panel, as elements: messages, the agent's plan, its tool calls and
 * the permissions waiting for an answer. Agent text is set as text; message HTML comes from the
 * store's sanitizing Markdown cache.
 */
export function conversationElements(
  doc: Document,
  state: AgentState,
  button: ButtonFactory,
  actions: {
    openLocation: (path: string, line: number) => void;
    answer: (permissionId: string, allow: boolean) => void;
  },
): HTMLElement[] {
  const content: HTMLElement[] = state.messages.map((message) => {
    const article = doc.createElement("article");
    article.setAttribute("aria-label", message.role);
    article.innerHTML = message.html.join("");
    return article;
  });
  if (state.plan.length) content.push(planList(doc, state.plan));
  for (const tool of state.tools) content.push(toolCard(doc, tool, button, actions.openLocation));
  for (const permission of state.permissions) {
    const card = doc.createElement("article");
    card.className = "coloride-agent-permission";
    const text = doc.createElement("p");
    text.textContent = permission.reason;
    card.append(
      text,
      button(UI_STRINGS.agent.allow, () => actions.answer(permission.id, true)),
      button(UI_STRINGS.agent.reject, () => actions.answer(permission.id, false)),
    );
    content.push(card);
  }
  return content;
}

function planList(doc: Document, plan: AgentState["plan"]): HTMLElement {
  const list = doc.createElement("ul");
  list.setAttribute("aria-label", UI_STRINGS.agent.plan);
  for (const entry of plan) {
    const item = doc.createElement("li");
    const check = doc.createElement("input");
    check.type = "checkbox";
    check.checked = entry.status === "completed";
    check.disabled = true;
    check.setAttribute("aria-label", entry.content);
    item.append(check, doc.createTextNode(`${entry.content} (${entry.status})`));
    list.append(item);
  }
  return list;
}

function toolCard(
  doc: Document,
  tool: ToolCard,
  button: ButtonFactory,
  openLocation: (path: string, line: number) => void,
): HTMLElement {
  const card = doc.createElement("article");
  card.className = "coloride-agent-tool";
  card.dataset.status = tool.status;
  const title = doc.createElement("h3");
  const glyph = { pending: "!", running: "⟳", done: "•", denied: "✕", failed: "✕" }[tool.status];
  title.textContent = `${glyph} ${tool.title} · ${tool.kind} · ${tool.status}`;
  card.append(title);
  if (tool.reason) {
    const reason = doc.createElement("p");
    reason.textContent = tool.reason;
    card.append(reason);
  }
  for (const location of tool.locations)
    card.append(
      button(`${location.path}:${location.line}`, () => openLocation(location.path, location.line)),
    );
  if (tool.edit) {
    const diff = doc.createElement("pre");
    diff.setAttribute("aria-label", uiText.proposedDiff(tool.edit.path));
    diff.tabIndex = 0;
    const { path, baseContentHash, oldText, newText } = tool.edit;
    diff.textContent = `Base BLAKE3: ${baseContentHash}\n--- ${path}\n+++ proposed/${path}\n${formatDiff(diffLines(oldText, newText))}`;
    const label = doc.createElement("p");
    label.textContent = UI_STRINGS.agent.proposal;
    card.append(label, diff);
  }
  return card;
}
