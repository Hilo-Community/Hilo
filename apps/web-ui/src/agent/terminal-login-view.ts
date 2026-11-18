import type { TerminalView } from "../cockpit/terminal/terminal-pane.js";
import { createXtermView } from "../cockpit/terminal/xterm-view.js";
import type { AgentHandle, AgentState } from "./store.js";

/** Stable, transient terminal boundary. Its output never enters the conversation store. */
export class TerminalLoginView {
  readonly element: HTMLElement;
  private readonly heading: HTMLElement;
  private readonly details: HTMLElement;
  private readonly screen: HTMLElement;
  private readonly start: HTMLButtonElement;
  private view: TerminalView | null = null;
  private readonly observer: ResizeObserver | null;

  constructor(
    doc: Document,
    private readonly handle: () => AgentHandle | null,
    private readonly run: (action: () => Promise<void>) => void,
  ) {
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-permission coloride-agent-terminal-login";
    this.element.setAttribute("aria-label", "Isolated account sign-in terminal");
    this.heading = doc.createElement("strong");
    this.details = doc.createElement("pre");
    const explanation = doc.createElement("p");
    explanation.textContent =
      "Complete the agent's sign-in, then exit this terminal successfully. Coloride will reconnect the agent. Account tokens remain accessible to the agent in its private home.";
    this.screen = doc.createElement("div");
    this.screen.className = "coloride-agent-login-screen";
    this.start = doc.createElement("button");
    this.start.type = "button";
    this.start.textContent = "Start isolated sign-in";
    this.start.addEventListener("click", () =>
      run(async () => {
        const handle = this.handle();
        if (!handle?.startLogin) throw new Error("Interactive sign-in is unavailable.");
        this.start.disabled = true;
        this.view = createXtermView();
        this.view.open(this.screen);
        this.view.onData((data) =>
          this.run(async () => {
            const write = this.handle()?.writeLogin;
            if (!write) throw new Error("Sign-in terminal is no longer running.");
            await write(data);
          }),
        );
        await handle.startLogin();
        this.fit();
        this.view.focus();
      }),
    );
    const cancel = doc.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel sign-in";
    cancel.addEventListener("click", () =>
      run(async () => {
        await this.handle()?.stop();
      }),
    );
    this.element.append(this.heading, explanation, this.details, this.start, cancel, this.screen);
    this.observer =
      typeof ResizeObserver === "undefined" ? null : new ResizeObserver(() => this.fit());
    this.observer?.observe(this.screen);
    this.element.hidden = true;
  }
  render(request: AgentState["terminalAuth"]): void {
    this.element.hidden = !request;
    if (!request) {
      this.view?.dispose();
      this.view = null;
      this.screen.replaceChildren();
      this.start.disabled = false;
      return;
    }
    this.heading.textContent = request.name;
    this.details.textContent = `Additional arguments: ${JSON.stringify(request.args)}\nEnvironment overrides: ${JSON.stringify(request.env)}`;
  }
  write(data: number[]): void {
    this.view?.write(Uint8Array.from(data));
  }
  fit(): void {
    if (this.element.isConnected && this.view && this.view.fit()) {
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
