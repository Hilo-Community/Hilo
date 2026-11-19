import { type PromptInput, createPromptInput } from "@coloride/editor-cm6";
import { SPINNER_ICON, agentMark } from "../settings/agent-marks.js";
import { UI_STRINGS } from "../ui/ui-strings.js";
import { authChoiceCard, authenticationCards } from "./auth-panel.js";
import { ConversationView } from "./conversation-view.js";
import { formatDuration } from "./duration.js";
import { OptionBar } from "./option-bar.js";
import { type AgentHandle, type AgentHost, type AgentState, AgentStore } from "./store.js";
import { TerminalLoginView } from "./terminal-login-view.js";
import { isWorktreePath } from "./tool-details.js";
import { TranscriptKeeper } from "./transcript-keeper.js";

const SVG =
  'xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"';
// A heavier stroke than the other icons so the arrow reads at 16px inside the round button.
// One path, not two: the disabled arrow is translucent and overlapping strokes would show lighter.
const SEND_ICON = `<svg ${SVG.replace('stroke-width="2"', 'stroke-width="2.75"')}><path d="M12 19V5M5 12l7-7 7 7"/></svg>`;
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
  /** Sign-in requests already opened, so each page opens once per session. */
  private readonly signInOpened = new Set<string>();
  private readonly unsubscribe: () => void;
  private readonly keeper: TranscriptKeeper | null;
  private readonly prompt: PromptInput;
  private readonly start: HTMLButtonElement;
  private readonly submit: HTMLButtonElement;
  private readonly cancel: HTMLButtonElement;
  private readonly output: HTMLElement;
  private readonly conversation = new ConversationView();
  private readonly error: HTMLElement;
  private readonly status: HTMLElement;
  private readonly busyMark: HTMLElement;
  private readonly busyTime: HTMLElement;
  private readonly options: OptionBar;
  private tick: ReturnType<typeof setInterval> | undefined;
  private readonly login: TerminalLoginView;
  private readonly form: HTMLFormElement;
  private readonly signInBar: HTMLElement;
  constructor(
    doc: Document,
    private readonly host: AgentHost,
    private readonly openLocation: (path: string, line: number) => Promise<void>,
    private readonly identity: { agentId: string; name: string },
    autoStart = true,
    store = new AgentStore(),
    /** `restore`: the panel comes from a previous run, so its saved conversation is shown. */
    history?: { chatId: string; restore: boolean },
  ) {
    this.store = store;
    this.keeper =
      history && host.transcripts
        ? new TranscriptKeeper(store, host.transcripts, history.chatId, identity.agentId)
        : null;
    this.login = new TerminalLoginView(
      doc,
      () => this.handle,
      (action) => this.run(action),
    );
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-panel";
    this.element.setAttribute("aria-label", identity.name);
    this.start = this.button(UI_STRINGS.agent.connect, () => void this.begin());
    this.start.className = "coloride-agent-connect";
    this.output = doc.createElement("div");
    this.output.className = "coloride-agent-output";
    // A file the agent mentions (`[App.jsx](<src/App.jsx:6>)`) opens in the editor at that line.
    const openReference = (event: Event) => {
      const ref = (event.target as Element | null)?.closest<HTMLElement>(".coloride-agent-ref");
      const path = ref?.dataset.path;
      const line = Number(ref?.dataset.line);
      if (path && isWorktreePath(path) && Number.isSafeInteger(line) && line > 0) {
        event.preventDefault();
        this.run(() => this.openLocation(path, line));
      }
    };
    this.output.addEventListener("click", openReference);
    // Text that acts as a link opens with Enter, as a link does.
    this.output.addEventListener("keydown", (event) => {
      if (event.key === "Enter") openReference(event);
    });
    this.error = doc.createElement("p");
    this.error.setAttribute("role", "status");
    this.status = doc.createElement("div");
    this.status.className = "coloride-agent-status";
    this.status.append(this.error, this.start);
    const form = doc.createElement("form");
    this.form = form;
    form.className = "coloride-agent-composer";
    // Drawn by CodeMirror, not a native textarea: a thin caret like the editor's.
    // Enter sends, Shift+Enter makes a new line; never while an input method is composing.
    this.prompt = createPromptInput(form, {
      placeholder: UI_STRINGS.agent.ask,
      ariaLabel: UI_STRINGS.agent.prompt,
      maxLength: 24 * 1024,
      maxHeight: MAX_COMPOSER_HEIGHT,
      onSubmit: () => form.requestSubmit(),
      onFocus: () => this.activate(),
      // Several lines: the text takes the whole width and the agent and send button go under it.
      onMultilineChange: (multiline) => form.classList.toggle("is-multiline", multiline),
    });
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
      const text = this.prompt.getText().trim();
      if (!this.handle || !this.store.getState().ready || this.store.getState().busy || !text)
        return;
      this.store.setState((s) => ({ ...s, busy: true }));
      this.run(async () => {
        await this.handle?.prompt(text);
        this.prompt.setText("");
      });
    });
    // Who answers, and the send / stop button, on the line under the text.
    const bar = doc.createElement("div");
    bar.className = "coloride-agent-bar";
    const who = doc.createElement("span");
    who.className = "coloride-agent-who";
    who.append(agentMark(doc, identity.agentId, identity.name), identity.name);
    bar.append(who, this.submit, this.cancel);
    form.append(bar);
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
    this.busyTime = doc.createElement("span");
    this.busyTime.className = "coloride-agent-busy-time";
    this.options = new OptionBar(doc, this.element, (optionId, value) =>
      this.run(async () => {
        await this.handle?.setOption?.(optionId, value);
      }),
    );
    const end = doc.createElement("span");
    end.className = "coloride-agent-meta-end";
    end.append(this.options.element, this.busyTime, this.busyMark);
    meta.append(sandbox, end);
    this.signInBar = doc.createElement("div");
    this.signInBar.className = "coloride-agent-auth-bar";
    this.signInBar.hidden = true;
    this.element.append(this.output, this.signInBar, this.status, form, meta);
    this.unsubscribe = store.subscribe(() => {
      this.render();
      this.keeper?.changed();
    });
    this.render();
    if (history?.restore) this.keeper?.restore();
    if (autoStart) void this.begin();
  }
  /** Restored sessions stay dormant until an explicit user interaction. */
  activate(): void {
    if (!this.handle && !this.store.getState().error) void this.begin();
  }
  /** Whether the Host can sign this agent out (the panel's menu offers it only then). */
  canSignOut(): boolean {
    return this.host.signOut !== undefined;
  }
  /** Stops the session and starts the agent again. */
  reload(): void {
    void this.begin();
  }
  /** Signs the agent out; the panel then waits for Connect, which asks how to sign in. */
  logOut(): void {
    this.run(async () => {
      await this.host.signOut?.(this.identity.agentId);
    });
  }
  /** Signs the agent out and connects again, which asks how to sign in. */
  reauthenticate(): void {
    this.run(async () => {
      await this.host.signOut?.(this.identity.agentId);
      await this.begin();
    });
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
    this.signInOpened.clear();
    this.starting = true;
    this.render();
    try {
      await this.handle?.stop();
      if (generation !== this.generation) return;
      this.handle = null;
      // A new connection starts a new agent session; the conversation on screen stays.
      this.store.reconnect();
      const handle = await this.host.start({ installedAgentId: this.identity.agentId }, (event) => {
        if (generation === this.generation) {
          if (event.type === "terminalAuthOutput") this.login.write(event.data);
          else this.store.enqueue(event);
        }
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
    this.renderControls(s);
    this.conversation.render(this.output, s, (label, click) => this.button(label, click), {
      openLocation: (path, line) => this.run(() => this.openLocation(path, line)),
      answer: (id, allow) =>
        this.run(async () => {
          await this.handle?.answer(id, allow);
          this.store.answered(id);
        }),
    });
    this.renderSignIn(s);
    this.login.fit();
    this.openSignInPages();
  }
  /** The status line, the connect button, the composer and the busy mark. */
  private renderControls(s: AgentState): void {
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
    // No input until there is a session to type to: signing in comes first.
    this.form.hidden = !s.ready;
    this.submit.disabled = !s.ready || s.busy;
    this.cancel.disabled = !s.ready;
    // Send turns into Stop while the agent works.
    this.submit.hidden = s.busy;
    this.cancel.hidden = !s.busy;
    this.busyMark.hidden = !(s.busy || this.starting || (this.handle && !s.ready));
    this.options.update(s.options, s.busy || !this.handle?.setOption);
    this.showElapsed(s);
    // Nothing said yet: a roomy composer in the middle; afterwards a compact one at the bottom.
    const started =
      s.messages.length +
        s.plan.length +
        s.tools.length +
        s.permissions.length +
        s.elicitations.length >
        0 || s.terminalAuth !== null;
    this.element.dataset.mode = started ? "chat" : "empty";
    this.prompt.setPlaceholder(started ? UI_STRINGS.agent.followUp : UI_STRINGS.agent.ask);
  }
  /** While the agent works, how long it has been at it; it ticks once a second. */
  private showElapsed(s: AgentState): void {
    const turn = s.turn;
    if (!s.busy || !turn || turn.endedAt !== undefined) {
      clearInterval(this.tick);
      this.tick = undefined;
      this.busyTime.textContent = "";
      return;
    }
    const show = () => {
      this.busyTime.textContent = formatDuration(Date.now() - turn.startedAt);
    };
    show();
    this.tick ??= setInterval(show, 1000);
  }
  /**
   * Signing in, at the bottom above the composer, not in the conversation: the choice of method,
   * the agent's prompts (a code to type, the wait) and the terminal login. While it shows, the
   * status line and the busy mark make way.
   */
  private renderSignIn(s: AgentState): void {
    const doc = this.element.ownerDocument;
    const prompts = authenticationCards(doc, this.store, this.handle, (action) => this.run(action));
    const choice =
      s.authChoice && !s.ready
        ? authChoiceCard(doc, this.identity.name, s.authChoice, (methodId) => {
            this.store.chose();
            this.run(async () => {
              await this.handle?.chooseAuth?.(methodId);
            });
          })
        : null;
    this.login.render(s.terminalAuth);
    const bar = [
      ...(choice ? [choice] : []),
      ...prompts,
      ...(s.terminalAuth ? [this.login.element] : []),
    ];
    this.signInBar.replaceChildren(...bar);
    this.signInBar.hidden = bar.length === 0;
    if (bar.length > 0) {
      this.status.hidden = true;
      this.busyMark.hidden = true;
    }
  }
  /**
   * Opens the sign-in page of each new request by itself: the user chose to connect, and the Host
   * opens only the address it kept for that request, on a host it approved. A failure leaves the
   * card with its button.
   */
  private openSignInPages(): void {
    const handle = this.handle;
    if (!handle?.elicit) return;
    for (const request of this.store.getState().elicitations) {
      if (request.accepted || this.signInOpened.has(request.id)) continue;
      this.signInOpened.add(request.id);
      const elicit = handle.elicit(request.id, "accept");
      void elicit
        .then(() => this.store.answeredElicitation(request.id, "accept"))
        .catch(() => undefined);
    }
  }
  /** The panel was closed for good: its saved conversation is deleted too. */
  discard(): void {
    this.keeper?.discard();
  }
  destroy(): void {
    this.keeper?.flush();
    clearInterval(this.tick);
    this.conversation.dispose();
    this.options.close();
    this.login.dispose();
    this.generation += 1;
    this.unsubscribe();
    this.prompt.destroy();
    this.store.dispose();
    void this.handle?.stop().catch(() => undefined);
    this.element.remove();
  }
}
