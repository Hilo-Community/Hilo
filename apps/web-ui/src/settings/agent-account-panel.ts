import type { InstalledAgent } from "./agent-registry.js";

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

/**
 * Settings only unlinks: signing in happens in the agent's chat, when it connects. This asks
 * before it removes anything and says exactly what goes: the agent's saved sign-in, and the
 * provider API key when one is saved (shared by every agent that uses that provider).
 */
export class AgentUnlinkPanel {
  readonly element: HTMLElement;
  private readonly content: HTMLElement;
  private readonly notice: HTMLElement;
  private disposed = false;
  private busy = false;
  constructor(
    doc: Document,
    private readonly host: AgentAccountHost,
    private readonly agent: InstalledAgent,
    private readonly onClose: () => void,
  ) {
    this.element = doc.createElement("section");
    this.element.className = "hilo-agent-account";
    this.element.setAttribute("aria-label", `Unlink ${agent.name}`);
    this.content = doc.createElement("div");
    this.notice = doc.createElement("p");
    this.notice.setAttribute("role", "status");
    this.element.append(this.content, this.notice);
    void this.load();
  }
  dispose(): void {
    this.disposed = true;
    this.element.remove();
  }
  private async load(): Promise<void> {
    try {
      const status = await this.host.status(this.agent.agentId);
      if (!this.disposed) this.render(status);
    } catch (error) {
      if (!this.disposed) this.notice.textContent = String(error);
    }
  }
  private render(status: AgentAccountStatus): void {
    const doc = this.element.ownerDocument;
    const saved = status.providers.filter((provider) => provider.keySaved);
    const explanation = doc.createElement("p");
    const parts = [`Signs ${this.agent.name} out and removes its saved sign-in.`];
    for (const provider of saved)
      parts.push(
        `Also removes the saved ${provider.name} API key, which every agent that uses ${provider.name} shares.`,
      );
    parts.push("The next time you connect, you will be asked to sign in again.");
    explanation.textContent = parts.join(" ");
    const actions = doc.createElement("div");
    actions.className = "hilo-registry-actions";
    const confirm = doc.createElement("button");
    confirm.type = "button";
    confirm.textContent = "Unlink";
    confirm.className = "hilo-registry-ghost";
    confirm.addEventListener("click", () => this.unlink(saved.map((provider) => provider.id)));
    const cancel = doc.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => this.onClose());
    actions.append(confirm, cancel);
    this.content.replaceChildren(explanation, actions);
  }
  private unlink(savedKeys: string[]): void {
    if (this.busy || this.disposed) return;
    this.busy = true;
    for (const button of this.element.querySelectorAll<HTMLButtonElement>("button"))
      button.disabled = true;
    this.notice.textContent = "Unlinking…";
    void (async () => {
      // Sessions of this agent stop first, then the saved sign-in and keys go.
      await this.host.signOut(this.agent.agentId);
      for (const provider of savedKeys) await this.host.setKey(provider, null);
    })()
      .then(() => {
        if (this.disposed) return;
        this.notice.textContent = `Unlinked. Connect ${this.agent.name} again to sign in.`;
        for (const button of this.element.querySelectorAll("button")) button.remove();
      })
      .catch((error: unknown) => {
        if (this.disposed) return;
        this.notice.textContent = String(error);
        for (const button of this.element.querySelectorAll<HTMLButtonElement>("button"))
          button.disabled = false;
      })
      .finally(() => {
        this.busy = false;
      });
  }
}
