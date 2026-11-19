import { SidebarMenu } from "../sidebar/sidebar-menu.js";
import { UI_STRINGS, uiText } from "../ui/ui-strings.js";
import type { AgentOption } from "./store.js";

/** The model first, then how hard it thinks, then the mode; anything else the agent offers after. */
const ORDER = ["model", "thought_level", "mode"];
const rank = (option: AgentOption) => {
  const index = ORDER.indexOf(option.category);
  return index < 0 ? ORDER.length : index;
};

/**
 * One small button per thing the agent lets the user choose (model, effort, mode), showing the
 * current choice; it opens the list of choices. The choices are whatever the agent offered: the
 * Host sends a pick back only if it is one of them.
 */
export class OptionBar {
  readonly element: HTMLElement;
  private readonly menu: SidebarMenu;
  private signature = "";
  constructor(
    private readonly doc: Document,
    menuHost: HTMLElement,
    private readonly pick: (optionId: string, value: string) => void,
  ) {
    this.element = doc.createElement("span");
    this.element.className = "coloride-agent-options";
    this.element.hidden = true;
    this.menu = new SidebarMenu(menuHost);
  }

  /** `locked`: a change is not possible now (a turn is running, or the Host cannot send one). */
  update(options: readonly AgentOption[], locked: boolean): void {
    const shown = [...options].sort((a, b) => rank(a) - rank(b));
    const signature = JSON.stringify(shown);
    if (signature !== this.signature) {
      this.signature = signature;
      this.menu.close(false);
      this.element.replaceChildren(...shown.map((option) => this.chip(option)));
    }
    this.element.hidden = shown.length === 0;
    for (const chip of this.element.querySelectorAll("button")) {
      chip.disabled = locked;
      if (locked) chip.setAttribute("aria-disabled", "true");
      else chip.removeAttribute("aria-disabled");
    }
  }

  close(): void {
    this.menu.close(false);
  }

  private chip(option: AgentOption): HTMLButtonElement {
    const chip = this.doc.createElement("button");
    chip.type = "button";
    chip.className = "coloride-agent-option";
    chip.dataset.option = option.id;
    const current = option.choices.find((choice) => choice.value === option.current);
    chip.textContent = current?.name ?? option.current;
    const label = uiText.agentOption(option.name, current?.name ?? option.current);
    chip.title = `${label} · ${UI_STRINGS.agent.optionLabel}`;
    chip.setAttribute("aria-label", label);
    chip.setAttribute("aria-haspopup", "menu");
    chip.addEventListener("click", () =>
      this.menu.open(
        chip,
        option.choices.map((choice) => ({
          label: choice.name,
          ...(choice.value === option.current ? { hint: "✓" } : {}),
          run: () => {
            if (choice.value !== option.current) this.pick(option.id, choice.value);
          },
        })),
      ),
    );
    return chip;
  }
}
