import { diffLines, formatDiff } from "./diff.js";
import { type AgentHandle, type AgentHost, AgentStore } from "./store.js";

/** Generic ACP panel. Its DOM and subscriptions never own or replace the CodeMirror view. */
export class AgentPanel {
  readonly element: HTMLElement;
  readonly store: AgentStore;
  private handle: AgentHandle | null = null;
  private generation = 0;
  private starting = false;
  private readonly unsubscribe: () => void;
  private readonly program: HTMLInputElement;
  private readonly args: HTMLInputElement;
  private readonly provider: HTMLInputElement;
  private readonly prompt: HTMLTextAreaElement;
  private readonly start: HTMLButtonElement;
  private readonly submit: HTMLButtonElement;
  private readonly cancel: HTMLButtonElement;
  private readonly output: HTMLElement;
  private readonly error: HTMLElement;
  constructor(
    doc: Document,
    private readonly host: AgentHost,
    private readonly openLocation: (path: string, line: number) => Promise<void>,
    store = new AgentStore(),
  ) {
    this.store = store;
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-panel";
    this.element.setAttribute("aria-label", "ACP agent");
    const field = (label: string, placeholder: string) => {
      const input = doc.createElement("input");
      input.setAttribute("aria-label", label);
      input.placeholder = placeholder;
      return input;
    };
    this.program = field("ACP executable", "Absolute path to ACP executable");
    this.args = field("Agent arguments as JSON", '["--acp"]');
    this.args.value = "[]";
    this.provider = field(
      "Provider destination",
      "Optional provider URL (requires Host egress support)",
    );
    this.start = this.button("Start agent", () => void this.begin());
    const setup = doc.createElement("details");
    setup.open = true;
    const summary = doc.createElement("summary");
    summary.textContent = "ACP agent configuration";
    const note = doc.createElement("p");
    note.textContent = "Offline, without credentials. Writes are proposals awaiting review.";
    setup.append(summary, this.program, this.args, this.provider, note, this.start);
    this.output = doc.createElement("div");
    this.output.className = "coloride-agent-output";
    this.error = doc.createElement("p");
    this.error.setAttribute("role", "status");
    const form = doc.createElement("form");
    this.prompt = doc.createElement("textarea");
    this.prompt.setAttribute("aria-label", "Agent prompt");
    this.prompt.placeholder = "Ask the agent…";
    this.prompt.maxLength = 24 * 1024;
    this.submit = this.button("Send prompt", () => undefined);
    this.submit.type = "submit";
    this.cancel = this.button("Cancel", () => this.run(() => this.handle?.cancel()));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const text = this.prompt.value.trim();
      if (!this.handle || !this.store.getState().ready || this.store.getState().busy || !text)
        return;
      this.store.setState((s) => ({ ...s, busy: true }));
      this.run(async () => {
        await this.handle?.prompt(text);
        this.prompt.value = "";
      });
    });
    form.append(this.prompt, this.submit, this.cancel);
    this.element.append(setup, this.output, this.error, form);
    this.unsubscribe = store.subscribe(() => this.render());
    this.render();
  }
  private button(label: string, click: () => void): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
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
    this.start.disabled = true;
    try {
      const args: unknown = JSON.parse(this.args.value);
      if (!Array.isArray(args) || args.some((a) => typeof a !== "string"))
        throw new Error("Arguments must be a JSON array of strings");
      await this.handle?.stop();
      this.handle = null;
      this.store.reset();
      const handle = await this.host.start(
        { program: this.program.value, args, provider: this.provider.value || null },
        (event) => {
          if (generation === this.generation) this.store.enqueue(event);
        },
      );
      if (generation !== this.generation) await handle.stop();
      else this.handle = handle;
    } catch (error) {
      this.store.setState((s) => ({ ...s, error: String(error) }));
    } finally {
      this.starting = false;
      this.start.disabled = false;
      this.render();
    }
  }
  private render(): void {
    const s = this.store.getState();
    const doc = this.element.ownerDocument;
    this.error.textContent = s.error;
    this.submit.disabled = !s.ready || s.busy;
    this.cancel.disabled = !s.ready;
    const content: HTMLElement[] = [];
    for (const message of s.messages) {
      const article = doc.createElement("article");
      article.setAttribute("aria-label", message.role);
      article.innerHTML = message.html.join("");
      content.push(article);
    }
    if (s.plan.length) {
      const plan = doc.createElement("ul");
      plan.setAttribute("aria-label", "Agent plan");
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
        diff.setAttribute("aria-label", `Read-only proposed diff for ${tool.edit.path}`);
        diff.tabIndex = 0;
        const { path, baseContentHash, oldText, newText } = tool.edit;
        diff.textContent = `Base BLAKE3: ${baseContentHash}\n--- ${path}\n+++ proposed/${path}\n${formatDiff(diffLines(oldText, newText))}`;
        const label = doc.createElement("p");
        label.textContent = "! Needs Review — proposal only";
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
        this.button("Allow once", () => answer(true)),
        this.button("Reject", () => answer(false)),
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
