import { AtomicStore } from "../state/atomic-store.js";
import type { AgentAccountHost } from "./agent-account-panel.js";

/** One agent of the public ACP registry, as the Host offers it on this computer. */
export interface RegistryAgent {
  readonly id: string;
  readonly name: string;
  readonly version: string;
  readonly description: string;
  readonly repository: string | null;
  readonly website: string | null;
  readonly license: string | null;
  readonly licenseUrl: string | null;
  readonly distribution: "binary" | "npm" | "uv";
  /** Why the agent cannot be installed here; null when it can. */
  readonly blocked: string | null;
}

export interface InstalledAgent {
  readonly id: string;
  readonly agentId: string;
  readonly name: string;
  readonly version: string;
  readonly distribution: string;
}

export interface AgentRegistryListing {
  readonly agents: readonly RegistryAgent[];
  readonly installed: readonly InstalledAgent[];
  /** `saved` when the registry could not be reached and an earlier catalog is shown. */
  readonly origin: "network" | "saved";
  readonly refreshError: string | null;
}

/**
 * What the Settings screen needs from the Host. Downloading and unpacking happen there; the
 * Host also asks the user to confirm an install in a native dialog.
 */
export interface AgentRegistryHost {
  account?: AgentAccountHost;
  list(refresh: boolean): Promise<AgentRegistryListing>;
  /** Null when the user declined the confirmation. */
  install(agentId: string, version: string): Promise<InstalledAgent | null>;
  uninstall(installationId: string): Promise<void>;
  open?(agentId: string): Promise<void>;
  onChanged?(listener: () => void): () => void;
  onOpenRequested?(listener: (agent: InstalledAgent) => void): () => void;
  /**
   * Called when the user confirms an install in the native dialog and the download starts.
   * Returns how to stop listening. A Host without it simply never reports a waiting phase.
   */
  onInstallConfirmed?(listener: (agentId: string) => void): () => void;
}

export type RegistryFilter = "all" | "installed" | "available";

export interface AgentRegistryState {
  readonly loading: boolean;
  readonly agents: readonly RegistryAgent[];
  readonly installed: readonly InstalledAgent[];
  readonly origin: AgentRegistryListing["origin"];
  readonly refreshError: string | null;
  readonly query: string;
  readonly filter: RegistryFilter;
  /** Agents with an install or removal running, by agent id. */
  readonly pending: Readonly<Record<string, "installing" | "removing">>;
  /** Installs still waiting for the user to answer the Host's confirmation, by agent id. */
  readonly awaiting: Readonly<Record<string, true>>;
  /** What went wrong for one agent, by agent id. */
  readonly failures: Readonly<Record<string, string>>;
  /** The listing itself could not be loaded. */
  readonly error: string;
  readonly notice: string;
}

/** An agent as a card: the catalog entry, or what is installed when the catalog dropped it. */
export interface RegistryRow {
  readonly agent: RegistryAgent;
  readonly installed: InstalledAgent | null;
  /** A different version than the installed one is on offer. */
  readonly updatable: boolean;
}

const INITIAL: AgentRegistryState = {
  loading: true,
  agents: [],
  installed: [],
  origin: "network",
  refreshError: null,
  query: "",
  filter: "all",
  pending: {},
  awaiting: {},
  failures: {},
  error: "",
  notice: "",
};

function message(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" && error ? error : "The operation failed. Please retry.";
}

/** The state of the ACP registry screen, and the operations that change it. */
export class AgentRegistryStore extends AtomicStore<AgentRegistryState> {
  private disposed = false;
  /** Only the latest listing request may write its result. */
  private listing = 0;

  private readonly stopListening: (() => void) | undefined;

  constructor(private readonly host: AgentRegistryHost) {
    super(INITIAL);
    this.stopListening = host.onInstallConfirmed?.((agentId) => this.confirmed(agentId));
  }

  dispose(): void {
    this.disposed = true;
    this.stopListening?.();
  }

  private confirmed(agentId: string): void {
    if (this.getState().awaiting[agentId])
      this.patch({ awaiting: without(this.getState().awaiting, agentId) });
  }

  private patch(change: Partial<AgentRegistryState>): void {
    if (!this.disposed) this.setState((state) => ({ ...state, ...change }));
  }

  setQuery(query: string): void {
    this.patch({ query });
  }

  setFilter(filter: RegistryFilter): void {
    this.patch({ filter });
  }

  /** Loads the catalog and the installed agents. `refresh` asks the Host to reach the registry. */
  async load(refresh = true): Promise<void> {
    const request = ++this.listing;
    // A reload after an install or removal is silent: showing "Loading agents…" for an instant
    // would push the whole list down and back up.
    if (refresh) this.patch({ loading: true });
    try {
      const listing = await this.host.list(refresh);
      if (request !== this.listing) return;
      // Without `refresh` the Host never reaches the registry and always answers from its saved
      // catalog; that says nothing about the registry, so what the last real refresh found stays.
      const { origin, refreshError } = this.getState();
      this.patch({
        loading: false,
        agents: listing.agents,
        installed: listing.installed,
        origin: refresh ? listing.origin : origin,
        refreshError: refresh ? listing.refreshError : refreshError,
        error: "",
      });
    } catch (error) {
      if (request === this.listing) this.patch({ loading: false, error: message(error) });
    }
  }

  private begin(agentId: string, kind: "installing" | "removing"): boolean {
    if (this.getState().pending[agentId]) return false;
    this.patch({
      pending: { ...this.getState().pending, [agentId]: kind },
      failures: without(this.getState().failures, agentId),
      notice: "",
    });
    return true;
  }

  private end(agentId: string): void {
    this.patch({
      pending: without(this.getState().pending, agentId),
      awaiting: without(this.getState().awaiting, agentId),
    });
  }

  /** Installs, or updates to, the version on offer. The Host confirms with the user first. */
  async install(agent: RegistryAgent): Promise<void> {
    if (agent.blocked || !this.begin(agent.id, "installing")) return;
    // Only a Host that reports the confirmation has a waiting phase to show.
    if (this.host.onInstallConfirmed)
      this.patch({ awaiting: { ...this.getState().awaiting, [agent.id]: true } });
    try {
      const done = await this.host.install(agent.id, agent.version);
      if (done) {
        this.patch({ notice: `${agent.name} ${done.version} installed.` });
        await this.load(false);
      }
    } catch (error) {
      this.patch({ failures: { ...this.getState().failures, [agent.id]: message(error) } });
    } finally {
      this.end(agent.id);
    }
  }

  async uninstall(installed: InstalledAgent): Promise<void> {
    if (!this.begin(installed.agentId, "removing")) return;
    try {
      await this.host.uninstall(installed.id);
      this.patch({ notice: `${installed.name} removed.` });
      await this.load(false);
    } catch (error) {
      this.patch({
        failures: { ...this.getState().failures, [installed.agentId]: message(error) },
      });
    } finally {
      this.end(installed.agentId);
    }
  }

  async open(installed: InstalledAgent): Promise<void> {
    try {
      await this.host.open?.(installed.agentId);
    } catch (error) {
      this.patch({
        failures: { ...this.getState().failures, [installed.agentId]: message(error) },
      });
    }
  }
}

function without<T>(record: Readonly<Record<string, T>>, key: string): Record<string, T> {
  const { [key]: _removed, ...rest } = record;
  return rest;
}

/**
 * Agents of the public registry that are not offered here: little used, or not worth a place in
 * the list. One that is already installed stays visible, so it can still be removed.
 * "cursor" is here because the registry publishes no SHA-256 for its download, so it cannot be
 * verified or installed yet; take it out of the list once the registry does.
 */
export const HIDDEN_AGENT_IDS: ReadonlySet<string> = new Set([
  "agoragentic-acp",
  "amp-acp",
  "auggie",
  "autohand",
  "codebuddy-code",
  "cortex-code",
  "corust-agent",
  "crow-cli",
  "cursor",
  "deepagents",
  "devin",
  "dimcode",
  "dirac",
  "fast-agent",
  "gemini",
  "glm-acp-agent",
  "goose",
  "harn",
  "junie",
  "kimchi",
  "minion-code",
  "muse-code",
  "nova",
  "opencode-v2",
  "pi-acp",
  "poolside",
  "qoder",
  "sigit",
  "stakpak",
  "vtcode",
]);

/** The cards to show for a state: filtered, searched and sorted by name. */
export function visibleRows(state: AgentRegistryState): RegistryRow[] {
  const installedBy = new Map(state.installed.map((item) => [item.agentId, item]));
  const rows: RegistryRow[] = state.agents.map((agent) => {
    const installed = installedBy.get(agent.id) ?? null;
    return {
      agent,
      installed,
      updatable: installed !== null && installed.version !== agent.version,
    };
  });
  // Installed agents the catalog no longer lists can still be removed.
  for (const installed of state.installed) {
    if (state.agents.some((agent) => agent.id === installed.agentId)) continue;
    rows.push({
      agent: {
        id: installed.agentId,
        name: installed.name,
        version: installed.version,
        description: "Installed on this computer.",
        repository: null,
        website: null,
        license: null,
        licenseUrl: null,
        distribution: installed.distribution === "npm" ? "npm" : "binary",
        blocked: "No longer in the registry.",
      },
      installed,
      updatable: false,
    });
  }
  const needle = state.query.trim().toLocaleLowerCase();
  return rows
    .filter(({ agent, installed }) => {
      if (installed === null && HIDDEN_AGENT_IDS.has(agent.id)) return false;
      const matchesFilter =
        state.filter === "all" || (state.filter === "installed") === (installed !== null);
      return (
        matchesFilter &&
        `${agent.name} ${agent.id} ${agent.description}`.toLocaleLowerCase().includes(needle)
      );
    })
    .sort((a, b) => a.agent.name.localeCompare(b.agent.name));
}
