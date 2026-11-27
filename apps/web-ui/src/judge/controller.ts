import type { HiloEditorView } from "@hilo/editor-cm6";
import { HILO_STATE_META } from "@hilo/protocol-types";
import { type AcceptOutcome, JudgeReviewPanel } from "./review-panel.js";
import { JUDGE_STRINGS } from "./strings.js";
import type { JudgeEvent, JudgeHost, JudgeReview } from "./types.js";

export interface JudgeControllerOptions {
  readonly host: JudgeHost;
  readonly editorView: HiloEditorView;
  /** Where progress and "ready for review" notices go (above the editor, in Zone 4). */
  readonly banner: HTMLElement;
  /** Shows the review in place of the editor (the editor is hidden, never unmounted). */
  readonly showReview: (element: HTMLElement) => void;
  readonly closeReview: () => void;
  /** Shows a project file in the editor. */
  readonly openFile: (path: string) => Promise<void>;
}

/**
 * The Judge in the cockpit (`LOT-015`): progress of verifications, the reviews the Host has
 * ready, accepting them through the editor, and the states the Host gives each range.
 */
export class JudgeController {
  private readonly reviews = new Map<string, JudgeReview>();
  private panel: JudgeReviewPanel | null = null;
  private unlisten: (() => void) | null = null;

  constructor(
    private readonly doc: Document,
    private readonly options: JudgeControllerOptions,
  ) {
    options.banner.hidden = true;
  }

  async start(): Promise<void> {
    this.unlisten = await this.options.host.listen((event) => this.handle(event));
    for (const review of await this.options.host.reviews()) {
      this.reviews.set(review.reviewId, review);
      this.announce(review);
    }
  }

  /** The review shown, if any. */
  getPanel(): JudgeReviewPanel | null {
    return this.panel;
  }

  openReview(reviewId: string): void {
    const review = this.reviews.get(reviewId);
    if (!review) return;
    this.panel?.destroy();
    this.panel = new JudgeReviewPanel(this.doc, review, {
      accept: (path) => this.accept(review, path),
      reject: () => this.options.host.reject(review.reviewId),
      adjust: (note) => this.options.host.adjust(review.reviewId, note),
      close: () => this.closePanel(),
    });
    this.options.showReview(this.panel.element);
    this.setBanner(null);
  }

  /** The user went back to the editor: the review stays one click away. */
  reviewLeft(): void {
    const review = this.panel?.review;
    this.panel?.destroy();
    this.panel = null;
    if (review && this.reviews.has(review.reviewId)) this.announce(review);
  }

  /** Draws the Host's states for the document now in the editor (after a switch). */
  async refreshStates(): Promise<void> {
    const snapshot = this.options.editorView.getStoreSnapshot();
    const decorations = await this.options.host.documentStates(
      snapshot.documentId,
      snapshot.sessionGeneration,
    );
    const now = this.options.editorView.getStoreSnapshot();
    if (
      decorations.length > 0 &&
      now.documentId === snapshot.documentId &&
      now.sessionGeneration === snapshot.sessionGeneration &&
      now.revision === snapshot.revision
    ) {
      this.options.editorView.setValidationDecorations(decorations);
    }
  }

  dispose(): void {
    this.unlisten?.();
    this.unlisten = null;
    this.closePanel();
  }

  private handle(event: JudgeEvent): void {
    switch (event.type) {
      case "verifying":
        this.setBanner(JUDGE_STRINGS.verifying(event.attempt));
        break;
      case "firstVerdict": {
        const meta = HILO_STATE_META[event.state];
        this.setBanner(
          JUDGE_STRINGS.firstVerdict(meta.labelEn, (event.elapsedMs / 1000).toFixed(1)),
        );
        break;
      }
      case "retrying":
        this.setBanner(JUDGE_STRINGS.retrying(event.attempt));
        break;
      case "error":
        this.setBanner(event.reason);
        break;
      case "reviewReady":
        this.reviews.set(event.review.reviewId, event.review);
        if (this.panel) this.announce(event.review);
        else this.openReview(event.review.reviewId);
        break;
      case "reviewClosed":
        this.reviews.delete(event.reviewId);
        if (this.panel?.review.reviewId === event.reviewId) this.closePanel();
        if (this.options.banner.dataset.reviewId === event.reviewId) this.setBanner(null);
        break;
      case "states": {
        const snapshot = this.options.editorView.getStoreSnapshot();
        if (
          snapshot.documentId === event.documentId &&
          snapshot.sessionGeneration === event.sessionGeneration
        ) {
          this.options.editorView.setValidationDecorations(event.decorations);
        }
        break;
      }
    }
  }

  private async accept(review: JudgeReview, path: string): Promise<AcceptOutcome> {
    await this.options.openFile(path);
    // The Host plans on its revision; an edit still on its way to it means planning again.
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const snapshot = this.options.editorView.getStoreSnapshot();
      const response = await this.options.host.accept(
        review.reviewId,
        path,
        snapshot.documentId,
        snapshot.sessionGeneration,
      );
      if (response.type === "conflict") return response;
      if (
        this.options.editorView.applyAcceptedChange(
          snapshot.documentId,
          snapshot.sessionGeneration,
          response.baseRevision,
          response.changes,
          response.taskId,
        )
      ) {
        return response;
      }
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return { type: "moved" };
  }

  private closePanel(): void {
    if (!this.panel) return;
    this.panel.destroy();
    this.panel = null;
    this.options.closeReview();
  }

  private announce(review: JudgeReview): void {
    const meta = HILO_STATE_META[review.verdict];
    this.setBanner(JUDGE_STRINGS.ready(meta.labelEn), review.reviewId);
  }

  private setBanner(text: string | null, reviewId?: string): void {
    const banner = this.options.banner;
    banner.replaceChildren();
    delete banner.dataset.reviewId;
    banner.hidden = text === null;
    if (text === null) return;
    const message = this.doc.createElement("span");
    message.className = "hilo-judge-banner-text";
    message.textContent = text;
    banner.append(message);
    if (reviewId) {
      banner.dataset.reviewId = reviewId;
      const open = this.doc.createElement("button");
      open.type = "button";
      open.className = "hilo-judge-banner-open";
      open.textContent = JUDGE_STRINGS.open;
      open.addEventListener("click", () => this.openReview(reviewId));
      banner.append(open);
    }
    const dismiss = this.doc.createElement("button");
    dismiss.type = "button";
    dismiss.className = "hilo-judge-banner-dismiss";
    dismiss.textContent = "×";
    dismiss.setAttribute("aria-label", JUDGE_STRINGS.dismiss);
    dismiss.addEventListener("click", () => this.setBanner(null));
    banner.append(dismiss);
  }
}
