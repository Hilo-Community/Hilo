import { type Extension, Prec, StateEffect, StateField } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  type ViewUpdate,
  WidgetType,
  keymap,
} from "@codemirror/view";
import type { LanguageFeatureEditorState } from "./language-features.js";

/** What the host needs to read the text around the cursor from its own rope. */
export interface InlineCompletionRequest {
  readonly document_id: string;
  readonly session_generation: number;
  readonly revision: number;
  /** CodeMirror position: UTF-16 code units. */
  readonly position_utf16: number;
}

export interface InlineCompletionResponse {
  readonly document_id: string;
  readonly session_generation: number;
  readonly base_revision: number;
  readonly position_utf16: number;
  readonly text: string;
  readonly model_id: string;
  readonly latency_ms: number;
}

/** Asks the host's model gateway (`LOT-014`); `null` when there is nothing to suggest. */
export type InlineCompletionProvider = (
  request: InlineCompletionRequest,
) => Promise<InlineCompletionResponse | null>;

export interface InlineCompletionHost {
  current(): LanguageFeatureEditorState;
}

interface Ghost {
  readonly pos: number;
  readonly text: string;
}

const setGhost = StateEffect.define<Ghost | null>();

class GhostWidget extends WidgetType {
  constructor(private readonly text: string) {
    super();
  }
  override eq(other: GhostWidget): boolean {
    return other.text === this.text;
  }
  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-hilo-ghost";
    span.textContent = this.text;
    span.setAttribute("aria-hidden", "true");
    return span;
  }
  override ignoreEvent(): boolean {
    return true;
  }
}

/** The suggestion shown; any edit or cursor move clears it (it describes one exact text). */
const ghostField = StateField.define<Ghost | null>({
  create: () => null,
  update(value, tr) {
    for (const effect of tr.effects) {
      if (effect.is(setGhost)) return effect.value;
    }
    return tr.docChanged || tr.selection ? null : value;
  },
  provide: (field) =>
    EditorView.decorations.from(
      field,
      (ghost): DecorationSet =>
        ghost
          ? Decoration.set([
              Decoration.widget({ widget: new GhostWidget(ghost.text), side: 1 }).range(ghost.pos),
            ])
          : Decoration.none,
    ),
});

const ghostTheme = EditorView.baseTheme({
  ".cm-hilo-ghost": { opacity: "0.45", fontStyle: "italic", whiteSpace: "pre" },
});

/** Only at the end of what is written on the line: a suggestion mid-word reads as noise. */
function atLineEnd(view: EditorView, pos: number): boolean {
  const line = view.state.doc.lineAt(pos);
  return /^[\s)\]}>;,'"`]*$/.test(line.text.slice(pos - line.from));
}

/**
 * Inline AI completion as ghost text (`LOT-014`). Typing schedules one request after a short
 * pause; the host cancels any call in flight as soon as it receives the next keystroke, and an
 * answer is shown only if the editor is still at the exact revision and position it was for.
 * Tab accepts it as an ordinary edit, Escape dismisses it.
 */
export class InlineCompletionClient {
  private provider: InlineCompletionProvider | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private sequence = 0;
  private discarded = 0;

  constructor(
    private readonly host: InlineCompletionHost,
    private readonly debounceMs = 120,
  ) {}

  setProvider(provider: InlineCompletionProvider | null): void {
    this.provider = provider;
    this.cancelScheduled();
  }

  /** Answers dropped because the text or the cursor moved on before they arrived. */
  discardedAnswers(): number {
    return this.discarded;
  }

  extension(): Extension {
    return [
      ghostField,
      ghostTheme,
      Prec.highest(
        keymap.of([
          { key: "Tab", run: (view) => this.accept(view) },
          { key: "Escape", run: (view) => this.dismiss(view) },
        ]),
      ),
      EditorView.updateListener.of((update) => this.onUpdate(update)),
    ];
  }

  /** The suggestion currently shown, if any. */
  shown(view: EditorView): string | null {
    return view.state.field(ghostField, false)?.text ?? null;
  }

  accept(view: EditorView): boolean {
    const ghost = view.state.field(ghostField, false);
    if (!ghost) return false;
    view.dispatch({
      changes: { from: ghost.pos, insert: ghost.text },
      selection: { anchor: ghost.pos + ghost.text.length },
      effects: setGhost.of(null),
      userEvent: "input.complete",
      scrollIntoView: true,
    });
    return true;
  }

  dismiss(view: EditorView): boolean {
    if (!view.state.field(ghostField, false)) return false;
    view.dispatch({ effects: setGhost.of(null) });
    return true;
  }

  private cancelScheduled(): void {
    this.sequence += 1;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private onUpdate(update: ViewUpdate): void {
    if (!update.docChanged && !update.selectionSet) return;
    this.cancelScheduled();
    const typed = update.transactions.some(
      (tr) => tr.isUserEvent("input.type") || tr.isUserEvent("delete"),
    );
    const selection = update.state.selection;
    if (!typed || !this.provider || selection.ranges.length !== 1 || !selection.main.empty) {
      return;
    }
    const view = update.view;
    if (!atLineEnd(view, selection.main.head)) return;
    const sequence = this.sequence;
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.request(view, sequence);
    }, this.debounceMs);
  }

  private async request(view: EditorView, sequence: number): Promise<void> {
    const provider = this.provider;
    if (!provider) return;
    const asked = this.host.current();
    const pos = view.state.selection.main.head;
    let response: InlineCompletionResponse | null;
    try {
      response = await provider({
        document_id: asked.documentId,
        session_generation: asked.sessionGeneration,
        revision: asked.revision,
        position_utf16: pos,
      });
    } catch {
      // The local runtime is down or refused: the editor simply shows no suggestion.
      return;
    }
    if (!response) return;
    const now = this.host.current();
    const current =
      sequence === this.sequence &&
      response.document_id === now.documentId &&
      response.session_generation === now.sessionGeneration &&
      response.base_revision === asked.revision &&
      now.revision === asked.revision &&
      response.position_utf16 === pos &&
      view.state.selection.main.empty &&
      view.state.selection.main.head === pos;
    if (!current) {
      this.discarded += 1;
      return;
    }
    view.dispatch({ effects: setGhost.of({ pos, text: response.text }) });
  }
}
