import { fileTreeIconColor, fileTreeIconSvg } from "../theme/file-tree-icons.js";
import { TOOL_CHEVRON_ICON, TOOL_KIND_ICONS } from "../theme/icons.js";
import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import { highlightedDiffSource } from "./diff-highlighting.js";
import { type DiffLine, diffLines } from "./diff.js";
import { formatDuration } from "./duration.js";
import type { ToolCard } from "./store.js";
import type { FileDiff } from "./tool-details.js";

export type ToolVariant = "simple" | "command" | "diff";
export function toolVariant(kind: string): ToolVariant {
  if (kind === "execute") return "command";
  if (kind === "edit" || kind === "delete") return "diff";
  return "simple";
}
export function toolLabel(tool: ToolCard): { verb: string; target: string } {
  const verbs = UI_STRINGS.agent.toolVerbs;
  const kind = Object.hasOwn(verbs, tool.kind) ? (tool.kind as keyof typeof verbs) : "other";
  const tense = tool.status === "running" ? 1 : tool.status === "done" ? 2 : 0;
  const target = tool.edit?.path ?? tool.locations[0]?.path ?? tool.command ?? tool.title;
  return {
    verb: verbs[kind][tense],
    target: toolVariant(tool.kind) === "diff" ? baseName(target) : target,
  };
}
/** The files a tool changed: what the agent reported, or the proposal the Host holds. */
export function toolDiffs(tool: ToolCard): FileDiff[] {
  if (tool.diffs?.length) return tool.diffs;
  const edit = tool.edit;
  return edit ? [{ path: edit.path, oldText: edit.oldText, newText: edit.newText }] : [];
}
const baseName = (path: string) => path.slice(path.lastIndexOf("/") + 1);
/** A diff longer than this is shown short, with a button for the rest. */
const PREVIEW_ROWS = 18;
function el<K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  className: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = doc.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** Persistent disclosure shell: lifecycle changes never reset the user's choice or focus. */
export class CardView {
  readonly element: HTMLElement;
  readonly header: HTMLButtonElement;
  readonly body: HTMLElement;
  readonly label: HTMLElement;
  readonly target: HTMLElement;
  readonly extra: HTMLElement;
  private readonly icon: HTMLElement;
  private readonly disclosure: HTMLElement;
  private readonly chevron: HTMLElement;
  private readonly state: HTMLElement;
  private userOpen: boolean | undefined;
  constructor(doc: Document) {
    this.element = el(doc, "article", "coloride-agent-card");
    this.header = el(doc, "button", "coloride-agent-card-header");
    this.header.type = "button";
    this.icon = el(doc, "span", "coloride-agent-card-icon");
    this.icon.setAttribute("aria-hidden", "true");
    const copy = el(doc, "span", "coloride-agent-card-copy");
    this.label = el(doc, "span", "coloride-agent-card-verb");
    this.target = el(doc, "span", "coloride-agent-card-target");
    copy.append(this.label, this.target);
    this.extra = el(doc, "span", "coloride-agent-card-extra");
    this.state = el(doc, "span", "coloride-agent-card-state");
    this.chevron = el(doc, "span", "coloride-agent-card-chevron");
    this.chevron.setAttribute("aria-hidden", "true");
    this.chevron.innerHTML = TOOL_CHEVRON_ICON;
    this.header.append(this.icon, copy, this.extra, this.state, this.chevron);
    this.disclosure = el(doc, "div", "coloride-agent-card-disclosure");
    const clip = el(doc, "div", "coloride-agent-card-clip");
    this.body = el(doc, "div", "coloride-agent-card-body");
    clip.append(this.body);
    this.disclosure.append(clip);
    this.element.append(this.header, this.disclosure);
    this.header.addEventListener("click", () => {
      this.userOpen = this.header.getAttribute("aria-expanded") !== "true";
      this.setOpen(this.userOpen);
    });
  }
  update(
    kind: string,
    status: ToolCard["status"],
    verb: string,
    target: string,
    hasBody: boolean,
    autoOpen = false,
    /** A file card shows the icon of that file's type instead of the kind's. */
    file?: string,
  ): void {
    this.element.dataset.status = status;
    this.icon.className = `coloride-agent-card-icon${file ? ` explorer-icon ${fileTreeIconColor(file)}` : ""}`;
    this.icon.innerHTML = file
      ? fileTreeIconSvg(file)
      : ((Object.hasOwn(TOOL_KIND_ICONS, kind) ? TOOL_KIND_ICONS[kind] : TOOL_KIND_ICONS.other) ??
        "");
    this.label.textContent = verb;
    this.target.textContent = target;
    const stateLabel = UI_STRINGS.agent.toolStates[status];
    const detail = this.extra.getAttribute("aria-label");
    this.header.setAttribute(
      "aria-label",
      `${verb}${target ? ` ${target}` : ""} · ${stateLabel}${detail ? ` · ${detail}` : ""}`,
    );
    this.header.title = `${verb} ${target}`.trim();
    this.state.textContent = status === "denied" ? "■" : status === "pending" ? "!" : "";
    if (this.state.textContent) this.header.insertBefore(this.state, this.chevron);
    else this.state.remove();
    this.state.setAttribute("aria-hidden", "true");
    this.header.disabled = !hasBody;
    this.chevron.hidden = !hasBody;
    this.setOpen(hasBody && (this.userOpen ?? autoOpen));
  }
  private setOpen(open: boolean): void {
    this.element.dataset.open = String(open);
    this.header.setAttribute("aria-expanded", String(open));
    // Inert removes closed descendants from keyboard navigation while CSS animates the grid.
    this.disclosure.inert = !open;
    this.disclosure.setAttribute("aria-hidden", String(!open));
  }
}

export class ToolCardView {
  readonly card: CardView;
  private previous: ToolCard | undefined;
  private failed = false;
  /** Files whose long diff the reader opened in full; kept when the card updates. */
  private readonly expanded = new Set<string>();
  constructor(private readonly doc: Document) {
    this.card = new CardView(doc);
    this.card.element.classList.add("coloride-agent-tool");
  }
  update(tool: ToolCard, openLocation: (path: string, line: number) => void): HTMLElement {
    if (tool === this.previous) return this.card.element;
    const variant = toolVariant(tool.kind);
    const thinking = tool.kind === "think";
    this.card.element.classList.toggle("coloride-agent-activity", thinking);
    this.card.element.classList.toggle("coloride-agent-thinking", thinking);
    this.card.element.dataset.variant = variant;
    let { verb, target } = toolLabel(tool);
    const changes = (variant === "diff" ? toolDiffs(tool) : []).map((file) => {
      const lines = diffLines(file.oldText, file.newText);
      return {
        file,
        lines,
        added: lines.filter((line) => line.kind === "+").length,
        removed: lines.filter((line) => line.kind === "-").length,
      };
    });
    const first = changes[0]?.file;
    this.card.extra.replaceChildren();
    if (first) {
      // Keep the action verb, followed by the file's name or a count for several files.
      if (changes.length === 1) {
        target = baseName(first.path);
      } else {
        verb = `${verb} ${uiText.fileCount(changes.length)}`;
        target = "";
      }
      const added = changes.reduce((sum, change) => sum + change.added, 0);
      const removed = changes.reduce((sum, change) => sum + change.removed, 0);
      this.card.extra.setAttribute("aria-label", uiText.diffCount(added, removed));
      this.card.extra.append(...this.counts(added, removed));
    } else {
      this.card.extra.removeAttribute("aria-label");
      // A tool that took a while says so; the quick ones need no number.
      if (tool.durationMs !== undefined && tool.durationMs >= 1000)
        this.card.extra.append(
          el(this.doc, "span", "coloride-agent-card-time", formatDuration(tool.durationMs)),
        );
    }
    this.card.element.querySelector(".coloride-agent-locations.is-inline")?.remove();
    const body: HTMLElement[] = [];
    if (variant === "command") body.push(this.command(tool));
    else if (thinking && tool.output)
      body.push(el(this.doc, "p", "coloride-agent-thought-text", tool.output));
    else if (changes.length) {
      if (tool.edit && !tool.diffs?.length) {
        const note = el(this.doc, "p", "coloride-agent-card-note", UI_STRINGS.agent.proposal);
        note.title = `${UI_STRINGS.agent.baseRevision}: ${tool.edit.baseContentHash}`;
        body.push(note);
      }
      for (const change of changes) {
        if (changes.length > 1)
          body.push(this.fileHead(change.file.path, change.added, change.removed));
        body.push(...this.preview(change.file, change.lines));
      }
    }
    if (tool.reason) body.push(el(this.doc, "p", "coloride-agent-card-note", tool.reason));
    if (
      tool.arguments?.length &&
      variant === "simple" &&
      !["read", "fetch", "move", "think"].includes(tool.kind)
    ) {
      const args = el(this.doc, "dl", "coloride-agent-arguments");
      for (const [key, value] of tool.arguments)
        args.append(el(this.doc, "dt", "", key), el(this.doc, "dd", "", value));
      body.push(args);
    }
    if (tool.kind === "search" && tool.output)
      body.push(el(this.doc, "pre", "coloride-agent-list", tool.output));
    if (tool.locations.length) {
      const list = el(this.doc, "div", "coloride-agent-locations");
      for (const location of tool.locations) {
        const link = el(
          this.doc,
          "button",
          "coloride-agent-location",
          variant === "diff" ? baseName(location.path) : `${location.path}:${location.line}`,
        );
        link.type = "button";
        link.title = `${location.path}:${location.line}`;
        link.addEventListener("click", () => openLocation(location.path, location.line));
        list.append(link);
      }
      // Simple reads remain single rows; the file opens from a separate, adjacent action.
      if (variant === "simple" && tool.kind !== "search" && body.length === 0) {
        this.card.element.querySelector(".coloride-agent-locations")?.remove();
        list.classList.add("is-inline");
        for (const link of list.querySelectorAll("button")) {
          link.setAttribute("aria-label", link.textContent ?? "");
          link.title = link.textContent ?? "";
          link.textContent = "↗";
        }
        this.card.element.append(list);
      } else body.push(list);
    }
    if (tool.truncated)
      body.push(el(this.doc, "p", "coloride-agent-card-note", UI_STRINGS.agent.truncated));
    const scroll = this.card.body.querySelector(".coloride-agent-command-output");
    const scrollTop = scroll?.scrollTop ?? 0;
    // Output follows the command while the reader is at the bottom, and stays put once they scroll up.
    const atEnd = !scroll || scroll.scrollTop + scroll.clientHeight >= scroll.scrollHeight - 4;
    this.card.body.replaceChildren(...body);
    const nextScroll = this.card.body.querySelector(".coloride-agent-command-output");
    if (nextScroll) {
      nextScroll.scrollTop =
        tool.status === "running" && atEnd ? nextScroll.scrollHeight : scrollTop;
    }
    // A command opens on its first failure; subsequent updates preserve a manual toggle.
    this.failed ||= tool.status === "failed";
    const iconPath =
      variant === "diff" && changes.length <= 1
        ? (first?.path ?? tool.edit?.path ?? tool.locations[0]?.path)
        : undefined;
    this.card.update(
      tool.kind,
      tool.status,
      verb,
      target,
      body.length > 0,
      variant === "diff" ||
        // A command shows its output while it runs, and folds again when it is done.
        (variant === "command" && (this.failed || tool.status === "running")) ||
        (thinking ? tool.status === "running" : !!tool.reason),
      iconPath ? baseName(iconPath) : undefined,
    );
    if (iconPath) this.card.header.title = iconPath;
    this.previous = tool;
    return this.card.element;
  }
  private counts(added: number, removed: number): HTMLElement[] {
    return [
      el(this.doc, "span", "coloride-agent-diff-added", `+${added}`),
      el(this.doc, "span", "coloride-agent-diff-removed", `−${removed}`),
    ];
  }
  /** The row naming one of several files in a card, with its own counts. */
  private fileHead(path: string, added: number, removed: number): HTMLElement {
    const head = el(this.doc, "div", "coloride-agent-file-head");
    const icon = el(
      this.doc,
      "span",
      `coloride-agent-card-icon explorer-icon ${fileTreeIconColor(baseName(path))}`,
    );
    icon.innerHTML = fileTreeIconSvg(baseName(path));
    icon.setAttribute("aria-hidden", "true");
    const name = el(this.doc, "span", "coloride-agent-file-name", baseName(path));
    name.title = path;
    head.append(icon, name, el(this.doc, "span", "coloride-agent-card-extra"));
    head.lastElementChild?.append(...this.counts(added, removed));
    return head;
  }
  /** The diff of one file: short at first when it is long, with a button for the rest. */
  private preview(file: FileDiff, lines: DiffLine[]): HTMLElement[] {
    const pre = diffView(this.doc, lines, file.path, file);
    const out: HTMLElement[] = [pre];
    if (lines.length > PREVIEW_ROWS) {
      const more = el(this.doc, "button", "coloride-agent-diff-more");
      more.type = "button";
      more.innerHTML = TOOL_CHEVRON_ICON;
      const show = (expanded: boolean) => {
        pre.dataset.expanded = String(expanded);
        more.dataset.expanded = String(expanded);
        const label = expanded ? UI_STRINGS.agent.showLess : UI_STRINGS.agent.showAll;
        more.setAttribute("aria-label", label);
        more.title = label;
        more.setAttribute("aria-expanded", String(expanded));
        if (expanded) this.expanded.add(file.path);
        else this.expanded.delete(file.path);
      };
      show(this.expanded.has(file.path));
      more.addEventListener("click", () => show(pre.dataset.expanded !== "true"));
      out.push(more);
    }
    if (file.truncated)
      out.push(el(this.doc, "p", "coloride-agent-card-note", UI_STRINGS.agent.truncated));
    return out;
  }
  private command(tool: ToolCard): HTMLElement {
    const block = el(this.doc, "div", "coloride-agent-command");
    const source = el(this.doc, "pre", "coloride-agent-command-source");
    source.setAttribute("aria-label", UI_STRINGS.agent.command);
    source.append(
      el(this.doc, "span", "coloride-agent-command-prompt", "$ "),
      this.doc.createTextNode(tool.command ?? tool.title),
    );
    block.append(source);
    if (tool.output) {
      const fade = el(this.doc, "div", "coloride-agent-output-fades");
      const output = el(this.doc, "pre", "coloride-agent-command-output", tool.output);
      output.tabIndex = 0;
      output.setAttribute("aria-label", UI_STRINGS.agent.output);
      fade.append(output);
      block.append(fade);
    }
    // A result line only when something went wrong: a command that worked needs no "Done".
    const result =
      tool.status === "denied"
        ? UI_STRINGS.agent.stopped
        : tool.exitCode !== undefined && tool.exitCode !== 0
          ? uiText.exitCode(tool.exitCode)
          : tool.status === "failed"
            ? UI_STRINGS.agent.toolStates.failed
            : "";
    if (result) block.append(el(this.doc, "p", "coloride-agent-command-result", result));
    return block;
  }
}

/** A read-only diff; skipped context advances both UTF-independent line counters. */
export function diffView(
  doc: Document,
  lines: DiffLine[],
  path: string,
  source?: Pick<FileDiff, "oldText" | "newText">,
): HTMLElement {
  const pre = el(doc, "pre", "coloride-agent-diff");
  pre.tabIndex = 0;
  pre.setAttribute("aria-label", uiText.proposedDiff(path));
  let oldLine = 1;
  let newLine = 1;
  // Parse both revisions separately: deleted text cannot change the new revision's grammar.
  const before = source ? highlightedDiffSource(source.oldText, path) : [];
  const after = source ? highlightedDiffSource(source.newText, path) : [];
  if (!lines.length) pre.textContent = UI_STRINGS.agent.noChange;
  for (const line of lines) {
    const row = el(doc, "span", "coloride-agent-diff-line");
    row.dataset.kind = line.kind;
    if (line.kind === "…") {
      oldLine += line.skipped;
      newLine += line.skipped;
      row.append(el(doc, "span", "coloride-agent-diff-gap", uiText.unchangedLines(line.skipped)));
    } else {
      // One number per row: a removed line has its old number, any other its new one.
      const number = el(
        doc,
        "span",
        "coloride-agent-line-number",
        String(line.kind === "-" ? oldLine : newLine),
      );
      number.setAttribute("aria-hidden", "true");
      const prefix = el(doc, "span", "coloride-agent-diff-prefix", line.kind);
      const code = el(doc, "span", "coloride-agent-diff-text");
      const parts = line.kind === "-" ? before[oldLine - 1] : after[newLine - 1];
      if (parts?.length) {
        for (const part of parts) code.append(el(doc, "span", part.classes, part.text));
      } else code.textContent = line.text;
      if (line.kind !== "+") oldLine++;
      if (line.kind !== "-") newLine++;
      row.append(number, prefix, code);
    }
    row.append(doc.createTextNode("\n"));
    pre.append(row);
  }
  return pre;
}
