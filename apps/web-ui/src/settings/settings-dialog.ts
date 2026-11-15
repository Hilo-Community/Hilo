import { UI_STRINGS } from "../ui/ui-strings.js";
import { AgentRegistryPanel } from "./agent-registry-panel.js";
import type { AgentRegistryHost } from "./agent-registry.js";

/** What the Host offers the Settings screen. */
export interface SettingsHosts {
  readonly agentRegistry: AgentRegistryHost;
}

const FOCUSABLE = 'button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * The Settings screen, a modal over the cockpit. It is built when opened and removed when
 * closed, so each visit reads the registry afresh; the editor behind it is never touched.
 */
export class SettingsDialog {
  private overlay: HTMLElement | null = null;
  private panel: AgentRegistryPanel | null = null;
  private opener: HTMLElement | null = null;

  constructor(
    private readonly doc: Document,
    private readonly parent: HTMLElement,
    private readonly hosts: SettingsHosts,
  ) {}

  get isOpen(): boolean {
    return this.overlay !== null;
  }

  open(): void {
    if (this.overlay) return;
    const doc = this.doc;
    const text = UI_STRINGS.settings;
    this.opener = doc.activeElement instanceof HTMLElement ? doc.activeElement : null;

    const overlay = doc.createElement("div");
    overlay.className = "coloride-settings-overlay";
    const dialog = doc.createElement("div");
    dialog.className = "coloride-settings-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    dialog.setAttribute("aria-labelledby", "coloride-settings-title");

    const header = doc.createElement("header");
    const title = doc.createElement("h1");
    title.id = "coloride-settings-title";
    title.textContent = text.title;
    const close = doc.createElement("button");
    close.type = "button";
    close.className = "coloride-settings-close";
    close.setAttribute("aria-label", text.close);
    close.textContent = "✕";
    close.addEventListener("click", () => this.close());
    header.append(title, close);

    const body = doc.createElement("div");
    body.className = "coloride-settings-body";
    const nav = doc.createElement("nav");
    nav.setAttribute("aria-label", text.categories);
    const agents = doc.createElement("button");
    agents.type = "button";
    agents.textContent = text.agents;
    agents.setAttribute("aria-current", "page");
    nav.append(agents);
    const main = doc.createElement("main");
    this.panel = new AgentRegistryPanel(doc, this.hosts.agentRegistry);
    main.append(this.panel.element);
    body.append(nav, main);

    dialog.append(header, body);
    overlay.append(dialog);
    overlay.addEventListener("keydown", (event) => this.keydown(event, dialog));
    overlay.addEventListener("mousedown", (event) => {
      if (event.target === overlay) this.close();
    });
    this.parent.append(overlay);
    this.overlay = overlay;
    this.panel.focusSearch();
  }

  private keydown(event: KeyboardEvent, dialog: HTMLElement): void {
    if (event.key === "Escape") {
      event.stopPropagation();
      this.close();
      return;
    }
    if (event.key !== "Tab") return;
    // Keep Tab inside the dialog.
    const items = [...dialog.querySelectorAll<HTMLElement>(FOCUSABLE)];
    const first = items[0];
    const last = items[items.length - 1];
    if (!first || !last) return;
    const active = this.doc.activeElement;
    if (event.shiftKey && active === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  }

  close(): void {
    this.panel?.destroy();
    this.panel = null;
    this.overlay?.remove();
    this.overlay = null;
    this.opener?.focus();
    this.opener = null;
  }

  toggle(): void {
    if (this.isOpen) this.close();
    else this.open();
  }
}
