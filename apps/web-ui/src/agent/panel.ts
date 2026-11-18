import { SPINNER_ICON, agentMark } from "../settings/agent-marks.js";
import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import { diffLines, formatDiff } from "./diff.js";
import { type AgentHandle, type AgentHost, AgentStore } from "./store.js";

const SVG =
  'xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"';
const SEND_ICON = `<svg ${SVG}><path d="M12 19V5"/><path d="m5 12 7-7 7 7"/></svg>`;
const STOP_ICON =
  '<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><rect x="5" y="5" width="14" height="14" rx="2"/></svg>';
const SANDBOX_ICON = `<svg ${SVG}><rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>`;
/** The composer grows with its text up to this height, then scrolls. */
const MAX_COMPOSER_HEIGHT = 220;

/** Generic ACP panel. Its DOM and subscriptions never own or replace the CodeMirror view. */
export class AgentPanel {
  readonly element: HTMLElement;
  readonly store: AgentStore;
  private handle: AgentHandle | null = null;
  private generation = 0;
  private starting = false;
  private readonly unsubscribe: () => void;
  private readonly prompt: HTMLTextAreaElement;
  private readonly start: HTMLButtonElement;
  private readonly submit: HTMLButtonElement;
  private readonly cancel: HTMLButtonElement;
  private readonly output: HTMLElement;
  private readonly error: HTMLElement;
  private readonly status: HTMLElement;
  private readonly busyMark: HTMLElement;
  constructor(
    doc: Document,
    private readonly host: AgentHost,
    private readonly openLocation: (path: string, line: number) => Promise<void>,
    private readonly identity: { agentId: string; name: string },
    autoStart = true,
    store = new AgentStore(),
  ) {
    this.store = store;
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-panel";
    this.element.setAttribute("aria-label", identity.name);
    this.start = this.button(UI_STRINGS.agent.connect, () => void this.begin());
    this.start.className = "coloride-agent-connect";
    this.output = doc.createElement("div");
    this.output.className = "coloride-agent-output";
    this.error = doc.createElement("p");
    this.error.setAttribute("role", "status");
    this.status = doc.createElement("div");
    this.status.className = "coloride-agent-status";
    this.status.append(this.error, this.start);
    const form = doc.createElement("form");
    form.className = "coloride-agent-composer";
    this.prompt = doc.createElement("textarea");
    this.prompt.rows = 1;
    // No red or blue squiggles under what is typed: prompts are code and paths, not prose.
    this.prompt.spellcheck = false;
    this.prompt.setAttribute("autocorrect", "off");
    this.prompt.setAttribute("autocapitalize", "off");
    this.prompt.autocomplete = "off";
    this.prompt.setAttribute("aria-label", UI_STRINGS.agent.prompt);
    this.prompt.placeholder = UI_STRINGS.agent.ask;
    this.prompt.addEventListener("focus", () => this.activate());
    this.prompt.addEventListener("input", () => this.fit());
    // Enter sends, Shift+Enter makes a new line; never while an input method is composing.
    this.prompt.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
      event.preventDefault();
      form.requestSubmit();
    });
    this.prompt.maxLength = 24 * 1024;
    // Icon-only round buttons; their names stay "Send prompt" and "Cancel" for assistive tech.
    this.submit = this.button("", () => undefined, "coloride-agent-send");
    this.submit.type = "submit";
    this.submit.innerHTML = SEND_ICON;
    this.submit.setAttribute("aria-label", UI_STRINGS.agent.send);
    this.cancel = this.button(
      "",
      () => this.run(() => this.handle?.cancel()),
      "coloride-agent-stop",
    );
    this.cancel.innerHTML = STOP_ICON;
    this.cancel.setAttribute("aria-label", UI_STRINGS.agent.cancel);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const text = this.prompt.value.trim();
      if (!this.handle || !this.store.getState().ready || this.store.getState().busy || !text)
        return;
      this.store.setState((s) => ({ ...s, busy: true }));
      this.run(async () => {
        await this.handle?.prompt(text);
        this.prompt.value = "";
        this.fit();
      });
    });
    // Who answers, and the send / stop button, on the line under the text.
    const bar = doc.createElement("div");
    bar.className = "coloride-agent-bar";
    const who = doc.createElement("span");
    who.className = "coloride-agent-who";
    who.append(agentMark(doc, identity.agentId, identity.name), identity.name);
    bar.append(who, this.submit, this.cancel);
    form.append(this.prompt, bar);
    // The sandbox the agent runs in, and a spinner while it connects or works.
    const meta = doc.createElement("div");
    meta.className = "coloride-agent-meta";
    const sandbox = doc.createElement("span");
    sandbox.className = "coloride-agent-sandbox";
    sandbox.title = UI_STRINGS.agent.sandboxHint;
    sandbox.insertAdjacentHTML("afterbegin", SANDBOX_ICON);
    sandbox.append(UI_STRINGS.agent.sandbox);
    this.busyMark = doc.createElement("span");
    this.busyMark.className = "coloride-agent-busy";
    this.busyMark.setAttribute("aria-hidden", "true");
    this.busyMark.innerHTML = SPINNER_ICON;
    meta.append(sandbox, this.busyMark);
    this.element.append(this.output, this.status, form, meta);
    this.unsubscribe = store.subscribe(() => this.render());
    this.render();
    if (autoStart) void this.begin();
  }
  /** Restored sessions stay dormant until an explicit user interaction. */
  activate(): void {
    if (!this.handle && !this.store.getState().error) void this.begin();
  }
  /** The text field grows with what is typed, up to a limit. */
  private fit(): void {
    if (this.prompt.scrollHeight <= 0) return;
    this.prompt.style.height = "auto";
    this.prompt.style.height = `${Math.min(this.prompt.scrollHeight, MAX_COMPOSER_HEIGHT)}px`;
  }
  private button(label: string, click: () => void, className = ""): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
    if (className) button.className = className;
    button.textContent = label;
    button.addEventListener("click", click);
    return button;
  }
  private run(action: () => Promise<void> | undefined): void {
    void Promise.resolve()
      .then(action)
      .catch((error: unknown) =>
        this.store.setState((s) => ({ ...s, busy: false, error: String(error) })),
      );
  }
  private async begin(): Promise<void> {
    if (this.starting) return;
    const generation = ++this.generation;
    this.starting = true;
    this.render();
    try {
      await this.handle?.stop();
      if (generation !== this.generation) return;
      this.handle = null;
      this.store.reset();
      const handle = await this.host.start({ installedAgentId: this.identity.agentId }, (event) => {
        if (generation === this.generation) this.store.enqueue(event);
      });
      if (generation !== this.generation) await handle.stop();
      else this.handle = handle;
    } catch (error) {
      if (generation === this.generation)
        this.store.setState((s) => ({ ...s, error: String(error) }));
    } finally {
      this.starting = false;
      if (generation === this.generation) this.render();
    }
  }
  private render(): void {
    const s = this.store.getState();
    const doc = this.element.ownerDocument;
    this.error.textContent =
      s.error ||
      (this.starting || (this.handle && !s.ready)
        ? UI_STRINGS.agent.connecting
        : !this.handle && !s.ready
          ? UI_STRINGS.agent.restored
          : "");
    this.start.textContent = s.error ? UI_STRINGS.agent.retry : UI_STRINGS.agent.connect;
    this.start.disabled = this.starting;
    this.start.hidden = this.starting || (s.ready && !s.error);
    this.status.hidden = !this.error.textContent;
    this.submit.disabled = !s.ready || s.busy;
    this.cancel.disabled = !s.ready;
    // Send turns into Stop while the agent works.
    this.submit.hidden = s.busy;
    this.cancel.hidden = !s.busy;
    this.busyMark.hidden = !(s.busy || this.starting || (this.handle && !s.ready));
    // Nothing said yet: a roomy composer in the middle; afterwards a compact one at the bottom.
    const started = s.messages.length + s.plan.length + s.tools.length + s.permissions.length > 0;
    this.element.dataset.mode = started ? "chat" : "empty";
    this.prompt.placeholder = started ? UI_STRINGS.agent.followUp : UI_STRINGS.agent.ask;
    const content: HTMLElement[] = [];
    for (const message of s.messages) {
      const article = doc.createElement("article");
      article.setAttribute("aria-label", message.role);
      article.innerHTML = message.html.join("");
      content.push(article);
    }
    if (s.plan.length) {
      const plan = doc.createElement("ul");
      plan.setAttribute("aria-label", UI_STRINGS.agent.plan);
      for (const entry of s.plan) {
        const item = doc.createElement("li");
        const check = doc.createElement("input");
        check.type = "checkbox";
        check.checked = entry.status === "completed";
        check.disabled = true;
        check.setAttribute("aria-label", entry.content);
        item.append(check, doc.createTextNode(`${entry.content} (${entry.status})`));
        plan.append(item);
      }
      content.push(plan);
    }
    for (const tool of s.tools) {
      const card = doc.createElement("article");
      card.className = "coloride-agent-tool";
      card.dataset.status = tool.status;
      const title = doc.createElement("h3");
      const glyph = { pending: "!", running: "⟳", done: "•", denied: "✕", failed: "✕" }[
        tool.status
      ];
      title.textContent = `${glyph} ${tool.title} · ${tool.kind} · ${tool.status}`;
      card.append(title);
      if (tool.reason) {
        const reason = doc.createElement("p");
        reason.textContent = tool.reason;
        card.append(reason);
      }
      for (const location of tool.locations)
        card.append(
          this.button(`${location.path}:${location.line}`, () =>
            this.run(() => this.openLocation(location.path, location.line)),
          ),
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
      content.push(card);
    }
    for (const permission of s.permissions) {
      const card = doc.createElement("article");
      card.className = "coloride-agent-permission";
      const text = doc.createElement("p");
      text.textContent = permission.reason;
      const answer = (allow: boolean) =>
        this.run(async () => {
          await this.handle?.answer(permission.id, allow);
          this.store.answered(permission.id);
        });
      card.append(
        text,
        this.button(UI_STRINGS.agent.allow, () => answer(true)),
        this.button(UI_STRINGS.agent.reject, () => answer(false)),
      );
      content.push(card);
    }
    this.output.replaceChildren(...content);
  }
  destroy(): void {
    this.generation += 1;
    this.unsubscribe();
    this.store.dispose();
    void this.handle?.stop().catch(() => undefined);
    this.element.remove();
  }
}
