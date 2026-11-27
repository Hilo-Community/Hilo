// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  SHELL_RESIZE_DELAY_MS,
  type TerminalHost,
  TerminalPane,
  type TerminalView,
} from "./terminal-pane.js";

class SizedView implements TerminalView {
  cols = 80;
  rows = 24;
  next = { cols: 80, rows: 24 };
  open(): void {}
  write(): void {}
  onData(): void {}
  fit(): boolean {
    const changed = this.next.cols !== this.cols || this.next.rows !== this.rows;
    this.cols = this.next.cols;
    this.rows = this.next.rows;
    return changed;
  }
  focus(): void {}
  dispose(): void {}
}

async function startedPane() {
  const resizes: Array<[number, number]> = [];
  const host: TerminalHost = {
    open: async () => ({
      write: () => undefined,
      resize: (cols, rows) => resizes.push([cols, rows]),
      close: () => undefined,
    }),
  };
  const view = new SizedView();
  const pane = new TerminalPane(document, host, () => view);
  document.body.append(pane.element);
  await pane.ensureStarted();
  /** What a resize observer does when the panel changes size. */
  const resizeTo = async (cols: number, rows: number) => {
    view.next = { cols, rows };
    await pane.ensureStarted();
  };
  return { pane, resizes, resizeTo };
}

describe("Terminal pane resizing", () => {
  afterEach(() => {
    vi.useRealTimers();
    document.body.replaceChildren();
  });

  it("tells the shell only the size the panel settles on, not every step of a drag", async () => {
    vi.useFakeTimers();
    const { resizes, resizeTo } = await startedPane();
    for (let cols = 81; cols <= 120; cols += 1) {
      await resizeTo(cols, 30);
      vi.advanceTimersByTime(SHELL_RESIZE_DELAY_MS / 4);
    }
    expect(resizes).toEqual([]);
    vi.advanceTimersByTime(SHELL_RESIZE_DELAY_MS);
    expect(resizes).toEqual([[120, 30]]);
  });

  it("does not resize a shell after the terminal was closed", async () => {
    vi.useFakeTimers();
    const { pane, resizes, resizeTo } = await startedPane();
    await resizeTo(100, 40);
    pane.destroy();
    vi.advanceTimersByTime(SHELL_RESIZE_DELAY_MS * 2);
    expect(resizes).toEqual([]);
  });
});
