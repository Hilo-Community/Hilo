import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
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

/** A round mark with the initials of the agent, in a colour taken from its name. */
function agentMark(doc: Document, name: string): HTMLElement {
  const mark = doc.createElement("span");
  mark.className = "coloride-registry-mark";
  mark.setAttribute("aria-hidden", "true");
  const initials = name
    .split(/[\s-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((word) => word.charAt(0).toUpperCase())
    .join("");
  mark.textContent = initials || "?";
  let hue = 0;
  for (const char of name) hue = (hue * 31 + char.charCodeAt(0)) % 360;
  mark.style.setProperty("--mark-hue", String(hue));
  return mark;
}

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

  constructor(doc: Document, host: AgentRegistryHost, store = new AgentRegistryStore(host)) {
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
    toolbar.append(this.search, group);

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
    options: { disabled: boolean; primary?: boolean },
  ): HTMLButtonElement {
    const button = this.element.ownerDocument.createElement("button");
    button.type = "button";
    button.textContent = label;
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
    const name = doc.createElement("h3");
    name.textContent = agent.name;
    const version = doc.createElement("span");
    version.className = "coloride-registry-version";
    version.textContent = uiText.agentVersion(installed?.version ?? agent.version);
    heading.append(agentMark(doc, agent.name), name, version);
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
    card.append(description);

    if (updatable && installed) {
      const note = doc.createElement("p");
      note.className = "coloride-registry-note";
      note.textContent = uiText.agentUpdateAvailable(installed.version, agent.version);
      card.append(note);
    }
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
    if (!installed) {
      actions.append(
        this.button(
          pending === "installing" ? text.installing : text.install,
          uiText.installAgent(agent.name, agent.version),
          `${agent.id}:install`,
          () => void this.store.install(agent),
          { disabled: busy || agent.blocked !== null, primary: true },
        ),
      );
    } else {
      if (updatable && !agent.blocked) {
        actions.append(
          this.button(
            pending === "installing" ? text.installing : text.update,
            uiText.updateAgent(agent.name, agent.version),
            `${agent.id}:install`,
            () => void this.store.install(agent),
            { disabled: busy, primary: true },
          ),
        );
      }
      actions.append(
        this.button(
          pending === "removing" ? text.removing : text.remove,
          uiText.removeAgent(agent.name),
          `${agent.id}:remove`,
          () => void this.store.uninstall(installed),
          { disabled: busy },
        ),
      );
    }
    card.append(actions);
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

    const rows = visibleRows(state);
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
    this.unsubscribe();
    this.store.dispose();
    this.element.remove();
  }
}
