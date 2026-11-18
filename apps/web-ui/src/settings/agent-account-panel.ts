import type { AgentRegistryHost, InstalledAgent } from "./agent-registry.js";

export interface AccountPreferences {
  mode: "auto" | "apiKey" | "account" | "offline";
  provider: string | null;
  methodId: string | null;
  extraAllowedHosts: string[];
}
export interface AgentAccountStatus {
  preferences: AccountPreferences;
  providers: { id: string; name: string; keySaved: boolean }[];
  authMethods: {
    id: string;
    name: string;
    type?: string | null;
    _meta?: Record<string, unknown>;
  }[];
  knownProfile: boolean;
  runtimeNotes?: string[];
}
export interface AgentAccountHost {
  status(agentId: string): Promise<AgentAccountStatus>;
  setKey(provider: string, key: string | null): Promise<void>;
  save(agentId: string, preferences: AccountPreferences): Promise<void>;
  signOut(agentId: string): Promise<void>;
}

/** Independent DOM boundary: API keys exist only in a transient password input, never a store. */
export class AgentAccountPanel {
  readonly element: HTMLElement;
  private readonly content: HTMLElement;
  private readonly notice: HTMLElement;
  private disposed = false;
  private busy = false;
  constructor(
    doc: Document,
    private readonly host: AgentAccountHost,
    private readonly registry: AgentRegistryHost,
    private readonly agent: InstalledAgent,
  ) {
    this.element = doc.createElement("section");
    this.element.className = "coloride-agent-account";
    this.element.setAttribute("aria-label", `${agent.name} account`);
    this.content = doc.createElement("div");
    this.notice = doc.createElement("p");
    this.notice.setAttribute("role", "status");
    this.element.append(this.content, this.notice);
    void this.load();
  }
  dispose(): void {
    this.disposed = true;
    for (const input of this.element.querySelectorAll<HTMLInputElement>('input[type="password"]'))
      input.value = "";
    this.element.remove();
  }
  private async load(): Promise<void> {
    this.notice.textContent = "Loading account…";
    try {
      const status = await this.host.status(this.agent.agentId);
      if (this.disposed) return;
      this.render(status);
      this.notice.textContent = "";
    } catch (error) {
      if (!this.disposed) this.notice.textContent = String(error);
    }
  }
  private run(action: () => Promise<void>): void {
    if (this.busy || this.disposed) return;
    this.busy = true;
    this.element.setAttribute("aria-busy", "true");
    for (const button of this.element.querySelectorAll<HTMLButtonElement>("button"))
      button.disabled = true;
    void action()
      .then(() => this.load())
      .catch((error: unknown) => {
        if (!this.disposed) this.notice.textContent = String(error);
      })
      .finally(() => {
        this.busy = false;
        this.element.setAttribute("aria-busy", "false");
        for (const button of this.element.querySelectorAll<HTMLButtonElement>("button"))
          button.disabled = false;
      });
  }
  private button(label: string, action: () => Promise<void>): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", () => this.run(action));
    return button;
  }
  private render(status: AgentAccountStatus): void {
    const doc = this.element.ownerDocument;
    const title = doc.createElement("h4");
    title.textContent = "Account";
    const mode = doc.createElement("p");
    const labels = {
      auto: "Automatic",
      apiKey: "Provider API key",
      account: "Provider account",
      offline: "Offline",
    };
    mode.textContent = `Selected mode: ${labels[status.preferences.mode]}`;
    const children: HTMLElement[] = [title, mode];
    for (const text of status.runtimeNotes ?? []) {
      const note = doc.createElement("p");
      note.textContent = text;
      children.push(note);
    }
    const save = (preferences: Partial<AccountPreferences>) =>
      this.host.save(this.agent.agentId, { ...status.preferences, ...preferences });
    for (const provider of status.providers) {
      const group = doc.createElement("div");
      const label = doc.createElement("label");
      label.textContent = `${provider.name} API key${provider.keySaved ? " · Saved in OS credential store" : ""}`;
      const input = doc.createElement("input");
      input.type = "password";
      input.autocomplete = "off";
      input.spellcheck = false;
      input.setAttribute("aria-label", `${provider.name} API key`);
      label.append(input);
      const note = doc.createElement("p");
      note.textContent =
        "Shared by agents using this provider. The Host keeps the key in your OS credential store.";
      group.append(
        label,
        this.button("Save API key", async () => {
          if (!input.value.trim()) throw new Error("Enter an API key.");
          const operation = this.host.setKey(provider.id, input.value.trim());
          input.value = "";
          await operation;
          await save({ mode: "apiKey", provider: provider.id, methodId: null });
        }),
        note,
      );
      if (provider.keySaved) {
        group.append(
          this.button(`Use ${provider.name} API key`, () =>
            save({ mode: "apiKey", provider: provider.id, methodId: null }),
          ),
        );
        group.append(
          this.button(`Remove ${provider.name} key`, () => this.host.setKey(provider.id, null)),
        );
      }
      children.push(group);
    }
    const logins = status.authMethods.filter(
      (method) => !method._meta?.gateway && !method._meta?.["api-key"],
    );
    for (const method of logins) {
      children.push(
        this.button(`Sign in · ${method.name}`, async () => {
          await save({ mode: "account", methodId: method.id });
          await this.registry.open?.(this.agent.agentId);
        }),
      );
    }
    const explanation = doc.createElement("p");
    explanation.textContent = logins.length
      ? "Account sign-in is handled by the agent. Its tokens stay in its private home and are accessible to that agent, giving weaker isolation than an API key held by the Host."
      : "Start the agent to discover its supported sign-in methods. Account tokens stay in its private home, with weaker isolation than a Host-held API key.";
    children.push(explanation);
    if (!status.knownProfile) {
      const label = doc.createElement("label");
      label.textContent = "Allowed account hosts · one exact DNS name per line";
      const hosts = doc.createElement("textarea");
      hosts.value = status.preferences.extraAllowedHosts.join("\n");
      hosts.setAttribute("aria-label", "Allowed account hosts");
      label.append(hosts);
      const denied = doc.createElement("p");
      denied.textContent =
        "Network is denied until you explicitly allow hosts. IP addresses, private destinations and wildcards are refused.";
      children.push(
        label,
        denied,
        this.button("Save allowed hosts", () =>
          save({ extraAllowedHosts: hosts.value.split(/\s+/).filter(Boolean) }),
        ),
      );
    }
    children.push(
      this.button("Use automatic selection", () => save({ mode: "auto", methodId: null })),
    );
    children.push(this.button("Use offline mode", () => save({ mode: "offline" })));
    children.push(
      this.button("Sign out and clear agent tokens", () => this.host.signOut(this.agent.agentId)),
    );
    children.push(this.button("Refresh account", async () => {}));
    this.content.replaceChildren(...children);
  }
}
