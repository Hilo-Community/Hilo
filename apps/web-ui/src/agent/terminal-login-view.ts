import type { TerminalView } from "../cockpit/terminal/terminal-pane.js";
import { createXtermView } from "../cockpit/terminal/xterm-view.js";
import { UI_STRINGS } from "../ui/ui-strings.js";
import type { AgentHandle, AgentState } from "./store.js";

/**
 * Sign-in for an agent that only offers a terminal login, at the bottom of the panel: a small
 * terminal, and the Host opens the sign-in page it prints. It starts as soon as the user chose
 * the method. An agent that asks for extra arguments or environment shows them and keeps an
 * explicit start button. The terminal's output never enters the conversation store.
 */
export class TerminalLoginView {
  readonly element: HTMLElement;
  private readonly details: HTMLElement;
  private readonly screen: HTMLElement;
  private readonly start: HTMLButtonElement;
  private readonly again: HTMLButtonElement;
  private view: TerminalView | null = null;
  private started = false;
  private failed = false;
  private request: AgentState["terminalAuth"] = null;
  private readonly observer: ResizeObserver | null;

  constructor(
    doc: Document,
    private readonly handle: () => AgentHandle | null,
    private readonly run: (action: () => Promise<void>) => void,
  ) {
    const text = UI_STRINGS.agent;
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-terminal-login";
    this.element.setAttribute("aria-label", "Isolated account sign-in terminal");
    const title = doc.createElement("strong");
    title.textContent = text.signInTitle;
    const intro = doc.createElement("p");
    intro.textContent = text.signInTerminal;
    this.details = doc.createElement("pre");
    this.details.hidden = true;
    this.screen = doc.createElement("div");
    this.screen.className = "coloride-agent-login-screen";
    this.start = doc.createElement("button");
    this.start.type = "button";
    this.start.textContent = text.signInStart;
    this.start.addEventListener("click", () => this.begin());
    const cancel = doc.createElement("button");
    cancel.type = "button";
    cancel.textContent = text.cancel;
    cancel.addEventListener("click", () =>
      run(async () => {
        await this.handle()?.stop();
      }),
    );
    const actions = doc.createElement("div");
    actions.className = "coloride-agent-terminal-login-actions";
    this.again = doc.createElement("button");
    this.again.type = "button";
    this.again.textContent = text.signInOpenAgainShort;
    this.again.addEventListener("click", () =>
      run(async () => {
        await this.handle()?.openLogin?.();
      }),
    );
    actions.append(this.start, this.again, cancel);
    this.element.append(title, intro, this.details, this.screen, actions);
    this.observer =
      typeof ResizeObserver === "undefined" ? null : new ResizeObserver(() => this.fit());
    this.observer?.observe(this.screen);
    this.element.hidden = true;
  }

  private begin(): void {
    if (this.started) return;
    this.started = true;
    this.failed = false;
    this.start.hidden = true;
    this.again.hidden = false;
    this.screen.hidden = false;
    this.run(async () => {
      try {
        const handle = this.handle();
        if (!handle?.startLogin) throw new Error("Interactive sign-in is unavailable.");
        this.view = createXtermView();
        this.view.open(this.screen);
        this.view.fit();
        this.view.onData((data) =>
          this.run(async () => {
            const write = this.handle()?.writeLogin;
            if (!write) throw new Error("Sign-in terminal is no longer running.");
            await write(data);
          }),
        );
        if (this.view.cols >= 2 && this.view.rows >= 1) {
          await handle.resizeLogin?.(this.view.cols, this.view.rows);
        }
        await handle.startLogin();
        this.fit(true);
        this.view.focus();
      } catch (error) {
        // A failed start is not retried by itself: the button is the way to try again.
        this.started = false;
        this.failed = true;
        this.view?.dispose();
        this.view = null;
        this.screen.replaceChildren();
        this.render(this.request);
        throw error;
      }
    });
  }

  render(request: AgentState["terminalAuth"]): void {
    this.request = request;
    this.element.hidden = !request;
    if (!request) {
      this.view?.dispose();
      this.view = null;
      this.screen.replaceChildren();
      this.started = false;
      this.failed = false;
      return;
    }
    const extra = Object.keys(request.env).length > 0;
    this.details.hidden = !extra;
    this.details.textContent = extra
      ? `${request.name}\nAdditional arguments: ${JSON.stringify(request.args)}\nEnvironment overrides: ${JSON.stringify(request.env)}`
      : "";
    // Arguments or environment the agent asked for need the user's click.
    if (!extra && !this.failed) this.begin();
    this.start.hidden = this.started;
    this.again.hidden = !this.started;
    this.screen.hidden = !this.started;
  }
  write(data: number[]): void {
    this.view?.write(Uint8Array.from(data));
  }
  fit(force = false): void {
    if (!this.element.isConnected || !this.view) return;
    const changed = this.view.fit();
    if ((changed || force) && this.view.cols >= 2 && this.view.rows >= 1) {
      const { cols, rows } = this.view;
      this.run(async () => {
        await this.handle()?.resizeLogin?.(cols, rows);
      });
    }
  }
  dispose(): void {
    this.observer?.disconnect();
    this.view?.dispose();
    this.element.remove();
  }
}
