import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { createLezerHighlightingExtensions } from "./languages.js";

export type MergeLayout = "unified" | "side-by-side";

export interface MergeReviewContent {
  readonly path: string;
  /** What the file holds now (or the text the change was judged against). */
  readonly original: string;
  /** What the change would make of it: exactly the content the Judge verified. */
  readonly proposed: string;
}

/**
 * Review of a proposed change with `@codemirror/merge` (`LOT-015`): unified or side by side.
 * Both sides are read-only: accepting applies the verified content, never an edited one. These
 * views are the review's own; the singleton editor is not touched.
 */
export class MergeReviewView {
  readonly element: HTMLElement;
  private layout: MergeLayout;
  private content: MergeReviewContent;
  private unified: EditorView | null = null;
  private split: MergeView | null = null;

  constructor(doc: Document, content: MergeReviewContent, layout: MergeLayout = "unified") {
    this.element = doc.createElement("div");
    this.element.className = "hilo-merge-review";
    this.content = content;
    this.layout = layout;
    this.render();
  }

  getLayout(): MergeLayout {
    return this.layout;
  }

  setLayout(layout: MergeLayout): void {
    if (layout === this.layout) return;
    this.layout = layout;
    this.render();
  }

  setContent(content: MergeReviewContent): void {
    this.content = content;
    this.render();
  }

  /** The text shown as proposed (the modified side). */
  getProposedText(): string {
    if (this.split) return this.split.b.state.doc.toString();
    return this.unified?.state.doc.toString() ?? "";
  }

  /** The text shown as original. */
  getOriginalText(): string {
    if (this.split) return this.split.a.state.doc.toString();
    return this.content.original;
  }

  destroy(): void {
    this.unified?.destroy();
    this.split?.destroy();
    this.unified = null;
    this.split = null;
    this.element.replaceChildren();
  }

  private extensions(): Extension[] {
    return [
      EditorState.readOnly.of(true),
      EditorView.editable.of(false),
      EditorView.lineWrapping,
      createLezerHighlightingExtensions(this.content.path),
    ];
  }

  private render(): void {
    this.unified?.destroy();
    this.split?.destroy();
    this.unified = null;
    this.split = null;
    this.element.replaceChildren();
    this.element.dataset.layout = this.layout;
    const { original, proposed } = this.content;
    if (this.layout === "side-by-side") {
      this.split = new MergeView({
        a: { doc: original, extensions: this.extensions() },
        b: { doc: proposed, extensions: this.extensions() },
        parent: this.element,
        highlightChanges: true,
        gutter: true,
      });
      return;
    }
    this.unified = new EditorView({
      parent: this.element,
      state: EditorState.create({
        doc: proposed,
        extensions: [
          ...this.extensions(),
          unifiedMergeView({ original, mergeControls: false, highlightChanges: true }),
        ],
      }),
    });
  }
}
