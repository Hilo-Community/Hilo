import { button } from "./dom-utils.js";
import { UI_STRINGS } from "./ui-strings.js";

/** A transient, non-modal notice, outside panels that can be hidden or clipped. */
export class Toast {
  private readonly element: HTMLElement;
  private readonly message: HTMLElement;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(doc: Document) {
    this.element = doc.createElement("div");
    this.element.className = "hilo-toast";
    this.element.hidden = true;
    this.element.setAttribute("role", "status");
    this.element.setAttribute("aria-live", "polite");
    this.element.setAttribute("aria-atomic", "true");
    this.message = doc.createElement("span");
    const close = button(doc, "hilo-toast-close", UI_STRINGS.notifications.dismiss);
    close.textContent = "×";
    close.addEventListener("click", () => this.hide());
    this.element.append(this.message, close);
    doc.body.append(this.element);
  }

  show(message: string): void {
    clearTimeout(this.timer);
    this.message.textContent = message;
    this.element.hidden = false;
    this.timer = setTimeout(() => this.hide(), 5000);
  }

  hide(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.element.hidden = true;
  }

  destroy(): void {
    this.hide();
    this.element.remove();
  }
}
