import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import { AgentAccountPanel } from "./agent-account-panel.js";
import { SEARCH_ICON, SPINNER_ICON, agentMark } from "./agent-marks.js";
import {
  type AgentRegistryHost,
  AgentRegistryStore,
  type RegistryFilter,
  type RegistryRow,
  visibleRows,
} from "./agent-registry.js";

const FILTERS: readonly (readonly [RegistryFilter, string])[] = [
  ["all", UI_STRINGS.settings.filterAll],
  ["installed", UI_STRINGS.settings.filterInstalled],
  ["available", UI_STRINGS.settings.filterAvailable],
];

/**
 * The ACP registry of the Settings screen: search, filters and one card per agent with its
 * install, update and remove buttons. It never touches the editor.
 */
export class AgentRegistryPanel {
  readonly element: HTMLElement;
  readonly store: AgentRegistryStore;
  private readonly unsubscribe: () => void;
  private readonly search: HTMLInputElement;
  private readonly filters = new Map<RegistryFilter, HTMLButtonElement>();
  private readonly status: HTMLElement;
  private readonly alert: HTMLElement;
  private readonly list: HTMLElement;
  private readonly accounts = new Map<string, AgentAccountPanel>();

  constructor(
    doc: Document,
    private readonly host: AgentRegistryHost,
    store = new AgentRegistryStore(host),
  ) {
    this.store = store;
    const text = UI_STRINGS.settings;
    this.element = doc.createElement("section");
    this.element.className = "coloride-registry";
    this.element.setAttribute("aria-label", text.registryTitle);

    const title = doc.createElement("h2");
    title.textContent = text.registryTitle;
    const intro = doc.createElement("p");
    intro.className = "coloride-registry-intro";
    intro.textContent = text.registryIntro;

    const toolbar = doc.createElement("div");
    toolbar.className = "coloride-registry-toolbar";
    this.search = doc.createElement("input");
    this.search.type = "search";
    this.search.placeholder = text.searchPlaceholder;
    this.search.setAttribute("aria-label", text.search);
    this.search.addEventListener("input", () => store.setQuery(this.search.value));
    const group = doc.createElement("div");
    group.className = "coloride-registry-filters";
    group.setAttribute("role", "group");
    group.setAttribute("aria-label", text.filters);
    for (const [filter, label] of FILTERS) {
      const button = doc.createElement("button");
      button.type = "button";
      button.textContent = label;
      button.addEventListener("click", () => store.setFilter(filter));
      this.filters.set(filter, button);
      group.append(button);
    }
    const searchBox = doc.createElement("label");
    searchBox.className = "coloride-registry-search";
    searchBox.insertAdjacentHTML("afterbegin", SEARCH_ICON);
    searchBox.append(this.search);
    toolbar.append(searchBox, group);

    this.status = doc.createElement("p");
    this.status.className = "coloride-registry-status";
    this.status.setAttribute("role", "status");
    this.alert = doc.createElement("div");
    this.alert.className = "coloride-registry-alert";
    this.alert.setAttribute("role", "alert");
    this.list = doc.createElement("div");
    this.list.className = "coloride-registry-list";

    this.element.append(title, intro, toolbar, this.alert, this.status, this.list);
    this.unsubscribe = store.subscribe(() => this.render());
    this.render();
    void store.load(true);
  }

  private button(
    label: string,
    ariaLabel: string,
    key: string,
    click: () => void,
    options: {
      disabled: boolean;
      primary?: boolean;
      icon?: string;
      ghost?: boolean;
      busy?: boolean;
    },
  ): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
    // The icon is a constant of this app (agent-marks.ts), never agent-supplied text.
    if (options.icon) button.insertAdjacentHTML("afterbegin", options.icon);
    // A running install, update or removal: a spinner in place of the icon.
    if (options.busy) button.dataset.busy = "true";
    // A running operation shows only the spinner, so the button keeps its width.
    if (options.busy) button.title = label;
    else button.append(label);
    if (options.ghost) button.classList.add("coloride-registry-ghost");
    button.setAttribute("aria-label", ariaLabel);
    button.dataset.key = key;
    button.disabled = options.disabled;
    if (options.primary) button.classList.add("coloride-registry-primary");
    button.addEventListener("click", click);
    return button;
  }

  private card(row: RegistryRow): HTMLElement {
    const doc = this.element.ownerDocument;
    const state = this.store.getState();
    const text = UI_STRINGS.settings;
    const { agent, installed, updatable } = row;
    const pending = state.pending[agent.id];

    const card = doc.createElement("article");
    card.className = "coloride-registry-card";
    card.setAttribute("aria-label", agent.name);
    card.dataset.agent = agent.id;
    card.dataset.installed = String(installed !== null);

    const heading = doc.createElement("div");
    heading.className = "coloride-registry-title";
    // Name and version are one line of text: "Claude Agent v0.107.0".
    const name = doc.createElement("h3");
    const version = doc.createElement("span");
    version.className = "coloride-registry-version";
    version.textContent = uiText.agentVersion(installed?.version ?? agent.version);
    if (updatable && installed) {
      // An update shows as the new version, in green; the button below becomes Update.
      version.textContent = uiText.agentVersion(agent.version);
      version.classList.add("coloride-registry-version--update");
      version.title = uiText.agentUpdateAvailable(installed.version, agent.version);
    }
    name.append(`${agent.name} `, version);
    heading.append(agentMark(doc, agent.id, agent.name), name);
    if (agent.license) {
      const license = doc.createElement("span");
      license.className = "coloride-registry-license";
      license.textContent = agent.license;
      heading.append(license);
    }
    card.append(heading);

    const description = doc.createElement("p");
    description.className = "coloride-registry-description";
    description.textContent = agent.description;
    description.title = agent.description;
    card.append(description);

    // A blocked agent that is not installed says why; one that is installed can still be removed.
    if (agent.blocked) {
      const blocked = doc.createElement("p");
      blocked.className = "coloride-registry-blocked";
      blocked.textContent = `${text.notInstallable}: ${agent.blocked}`;
      card.append(blocked);
    }
    const failure = state.failures[agent.id];
    if (failure) {
      const error = doc.createElement("p");
      error.className = "coloride-registry-failure";
      error.setAttribute("role", "alert");
      error.textContent = failure;
      card.append(error);
    }

    const actions = doc.createElement("div");
    actions.className = "coloride-registry-actions";
    const busy = pending !== undefined;
    if (busy) {
      card.dataset.pending = pending;
      // Gray while the Host waits for the user's answer, green once it is working.
      card.dataset.phase = state.awaiting[agent.id] ? "waiting" : "running";
    }
    if (!installed) {
      actions.append(
        this.button(
          pending === "installing" ? text.installing : text.install,
          uiText.installAgent(agent.name, agent.version),
          `${agent.id}:install`,
          () => void this.store.install(agent),
          {
            disabled: busy || agent.blocked !== null,
            primary: true,
            // No icon at rest; a spinner while it runs.
            ...(pending === "installing" ? { icon: SPINNER_ICON, busy: true } : {}),
          },
        ),
      );
    } else {
      actions.append(
        this.button(
          text.openAgent,
          uiText.openAgent(installed.name),
          `${agent.id}:open`,
          () => void this.store.open(installed),
          { disabled: busy },
        ),
      );
      // One button at a time: Update while a newer version is on offer, Remove otherwise.
      if (updatable && !agent.blocked) {
        actions.append(
          this.button(
            pending === "installing" ? text.installing : text.update,
            uiText.updateAgent(agent.name, agent.version),
            `${agent.id}:install`,
            () => void this.store.install(agent),
            {
              disabled: busy,
              primary: true,
              ...(pending === "installing" ? { icon: SPINNER_ICON, busy: true } : {}),
            },
          ),
        );
      } else {
        actions.append(
          this.button(
            pending === "removing" ? text.removing : text.remove,
            uiText.removeAgent(agent.name),
            `${agent.id}:remove`,
            () => void this.store.uninstall(installed),
            {
              disabled: busy,
              ghost: true,
              ...(pending === "removing" ? { icon: SPINNER_ICON, busy: true } : {}),
            },
          ),
        );
      }
    }
    card.append(actions);
    if (installed && this.host.account) {
      actions.append(
        this.button(
          "Account",
          `${installed.name} account`,
          `${agent.id}:account`,
          () => {
            let account = this.accounts.get(installed.id);
            if (account) account.element.hidden = !account.element.hidden;
            else if (this.host.account) {
              account = new AgentAccountPanel(doc, this.host.account, this.host, installed);
              this.accounts.set(installed.id, account);
              card.append(account.element);
            }
          },
          { disabled: busy },
        ),
      );
      const account = this.accounts.get(installed.id);
      if (account) card.append(account.element);
    }
    return card;
  }

  private render(): void {
    const state = this.store.getState();
    const text = UI_STRINGS.settings;
    const doc = this.element.ownerDocument;
    // Buttons are rebuilt on every change: put the keyboard focus back on the same one.
    const focused =
      doc.activeElement instanceof HTMLElement ? doc.activeElement.dataset.key : undefined;

    for (const [filter, button] of this.filters)
      button.setAttribute("aria-pressed", String(state.filter === filter));

    const alert: Node[] = [];
    if (state.error) {
      alert.push(doc.createTextNode(state.error), this.retryButton());
    } else if (state.origin === "saved") {
      const note = doc.createElement("span");
      note.textContent = state.refreshError
        ? `${text.savedCatalog} (${state.refreshError})`
        : text.savedCatalog;
      alert.push(note, this.retryButton());
    }
    this.alert.replaceChildren(...alert);
    this.alert.hidden = alert.length === 0;
    this.status.textContent = state.loading ? text.loading : state.notice;
    // "Codex installed." is announced to screen readers only; the card already shows the result.
    this.status.dataset.kind = state.loading ? "loading" : "notice";

    const rows = visibleRows(state);
    for (const [id, account] of this.accounts) {
      if (!state.installed.some((agent) => agent.id === id)) {
        account.dispose();
        this.accounts.delete(id);
      }
    }
    const children: Node[] = rows.map((row) => this.card(row));
    if (!state.loading && rows.length === 0 && !state.error) {
      const empty = doc.createElement("p");
      empty.className = "coloride-registry-empty";
      empty.textContent = state.query
        ? text.emptySearch
        : state.filter === "installed"
          ? text.emptyInstalled
          : text.emptyAll;
      children.push(empty);
    }
    this.list.replaceChildren(...children);
    this.list.setAttribute("aria-busy", String(state.loading));
    if (focused) {
      for (const candidate of this.list.querySelectorAll<HTMLElement>("[data-key]")) {
        if (candidate.dataset.key === focused && !(candidate as HTMLButtonElement).disabled) {
          candidate.focus();
          break;
        }
      }
    }
  }

  private retryButton(): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
    button.textContent = UI_STRINGS.settings.retry;
    button.addEventListener("click", () => void this.store.load(true));
    return button;
  }

  focusSearch(): void {
    this.search.focus();
  }

  destroy(): void {
    for (const account of this.accounts.values()) account.dispose();
    this.accounts.clear();
    this.unsubscribe();
    this.store.dispose();
    this.element.remove();
  }
}
