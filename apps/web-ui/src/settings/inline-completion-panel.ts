export interface InlineCompletionSettings {
  enabled: boolean;
  /** `null`: the largest local model that fits in memory. */
  modelId: string | null;
}
export interface InlineCompletionStatus {
  settings: InlineCompletionSettings;
  activeModel: string | null;
  ollamaRunning: boolean;
  models: { id: string; name: string; installed: boolean; fitsInMemory: boolean }[];
  availableMemoryBytes: number;
  installHint: string | null;
}
export interface InlineCompletionHost {
  status(): Promise<InlineCompletionStatus>;
  save(settings: InlineCompletionSettings): Promise<void>;
}

const GIB = 1024 ** 3;

/**
 * Local AI completion (`LOT-014`): a model on this machine through Ollama. Nothing leaves the
 * computer and no account is needed; this panel only says what is ready and what is missing.
 */
export class InlineCompletionPanel {
  readonly element: HTMLElement;
  private readonly content: HTMLElement;
  private readonly notice: HTMLElement;
  private disposed = false;

  constructor(
    private readonly doc: Document,
    private readonly host: InlineCompletionHost,
  ) {
    this.element = doc.createElement("section");
    this.element.className = "hilo-inline-completion";
    this.element.setAttribute("aria-label", "Local AI completion");
    this.content = doc.createElement("div");
    this.notice = doc.createElement("p");
    this.notice.setAttribute("role", "status");
    this.element.append(this.content, this.notice);
    void this.load();
  }

  destroy(): void {
    this.disposed = true;
    this.element.remove();
  }

  private async load(): Promise<void> {
    try {
      const status = await this.host.status();
      if (!this.disposed) this.render(status);
    } catch (error) {
      if (!this.disposed) this.notice.textContent = String(error);
    }
  }

  private paragraph(text: string, className = ""): HTMLParagraphElement {
    const p = this.doc.createElement("p");
    p.textContent = text;
    if (className) p.className = className;
    return p;
  }

  private render(status: InlineCompletionStatus): void {
    const doc = this.doc;
    const title = doc.createElement("h2");
    title.textContent = "Local AI completion";
    const about = this.paragraph(
      "Suggests the rest of the line as you type, with a model running on this computer through Ollama. Your code never leaves the machine and no account is needed. Tab accepts a suggestion, Escape dismisses it.",
    );

    const enabled = doc.createElement("input");
    enabled.type = "checkbox";
    enabled.checked = status.settings.enabled;
    const enabledLabel = doc.createElement("label");
    enabledLabel.className = "hilo-inline-completion-toggle";
    enabledLabel.append(enabled, " Show suggestions while typing");

    const model = doc.createElement("select");
    model.setAttribute("aria-label", "Model");
    const automatic = doc.createElement("option");
    automatic.value = "";
    const recommended = status.models.find((m) => m.id === status.activeModel);
    automatic.textContent = recommended
      ? `Automatic (${recommended.name})`
      : "Automatic (no local model fits in memory)";
    model.append(automatic);
    for (const entry of status.models) {
      const option = doc.createElement("option");
      option.value = entry.id;
      const notes = [
        entry.installed ? "" : "not installed",
        entry.fitsInMemory ? "" : "needs more free memory",
      ].filter(Boolean);
      option.textContent = notes.length ? `${entry.name} — ${notes.join(", ")}` : entry.name;
      model.append(option);
    }
    model.value = status.settings.modelId ?? "";

    const save = () => {
      const settings = { enabled: enabled.checked, modelId: model.value || null };
      this.notice.textContent = "Saving…";
      this.host
        .save(settings)
        .then(() => this.load())
        .then(() => {
          if (!this.disposed) this.notice.textContent = "Saved.";
        })
        .catch((error: unknown) => {
          if (!this.disposed) this.notice.textContent = String(error);
        });
    };
    enabled.addEventListener("change", save);
    model.addEventListener("change", save);

    const state = doc.createElement("div");
    state.className = "hilo-inline-completion-state";
    const memory = `${(status.availableMemoryBytes / GIB).toFixed(1)} GB of memory free.`;
    if (!status.ollamaRunning) {
      state.append(
        this.paragraph(
          "Ollama is not running. Install it from ollama.com and start it.",
          "is-warning",
        ),
      );
    } else if (status.installHint) {
      state.append(this.paragraph("The model is not installed yet. Run:", "is-warning"));
      const code = doc.createElement("code");
      code.textContent = status.installHint;
      state.append(code);
    } else if (status.activeModel) {
      state.append(this.paragraph(`Ready: ${recommended?.name ?? status.activeModel}.`));
    }
    state.append(this.paragraph(memory));

    const refresh = doc.createElement("button");
    refresh.type = "button";
    refresh.textContent = "Check again";
    refresh.addEventListener("click", () => {
      this.notice.textContent = "";
      void this.load();
    });

    this.content.replaceChildren(title, about, enabledLabel, model, state, refresh);
  }
}
