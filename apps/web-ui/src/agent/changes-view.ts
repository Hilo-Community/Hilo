import { uiText } from "../ui/ui-strings.js";
import { diffLines } from "./diff.js";
import type { ToolCard } from "./store.js";
import { CardView } from "./tool-card-view.js";

export interface FileChange {
  path: string;
  added: number;
  removed: number;
}

/** The files a turn's edits touched, once each, with the lines added and removed where known. */
export function fileChanges(tools: readonly ToolCard[]): FileChange[] {
  const files = new Map<string, FileChange>();
  for (const tool of tools) {
    if (tool.kind !== "edit" && tool.kind !== "delete") continue;
    if (tool.status === "failed" || tool.status === "denied") continue;
    const path = tool.edit?.path ?? tool.locations[0]?.path;
    if (!path) continue;
    const change = files.get(path) ?? { path, added: 0, removed: 0 };
    if (tool.edit) {
      const lines = diffLines(tool.edit.oldText, tool.edit.newText);
      change.added += lines.filter((line) => line.kind === "+").length;
      change.removed += lines.filter((line) => line.kind === "-").length;
    }
    files.set(path, change);
  }
  return [...files.values()];
}

/** "3 files changed +20 −5": the summary at the end of a turn that edited several files. */
export class ChangesView {
  readonly card: CardView;
  private signature = "";
  constructor(private readonly doc: Document) {
    this.card = new CardView(doc);
    this.card.element.classList.add("hilo-agent-changes");
    this.card.element.dataset.variant = "diff";
  }
  update(changes: FileChange[], openLocation: (path: string, line: number) => void): HTMLElement {
    const signature = JSON.stringify(changes);
    if (signature === this.signature) return this.card.element;
    this.signature = signature;
    const added = changes.reduce((sum, change) => sum + change.added, 0);
    const removed = changes.reduce((sum, change) => sum + change.removed, 0);
    const counts = (a: number, r: number) => {
      const wrap = this.doc.createElement("span");
      wrap.className = "hilo-agent-card-extra";
      if (a || r) {
        const plus = this.doc.createElement("span");
        plus.className = "hilo-agent-diff-added";
        plus.textContent = `+${a}`;
        const minus = this.doc.createElement("span");
        minus.className = "hilo-agent-diff-removed";
        minus.textContent = `−${r}`;
        wrap.append(plus, minus);
      }
      return wrap;
    };
    this.card.extra.replaceChildren(...counts(added, removed).children);
    const rows = changes.map((change) => {
      const row = this.doc.createElement("button");
      row.type = "button";
      row.className = "hilo-agent-change-file";
      const name = this.doc.createElement("span");
      name.className = "hilo-agent-change-path";
      name.textContent = change.path;
      row.append(name, counts(change.added, change.removed));
      row.addEventListener("click", () => openLocation(change.path, 1));
      return row;
    });
    this.card.body.replaceChildren(...rows);
    this.card.update("edit", "done", uiText.filesChanged(changes.length), "", true, false);
    return this.card.element;
  }
}
