// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import type { CockpitFileTreeNode } from "../../stores/index.js";
import { FileTreePanel } from "./file-tree-panel.js";

const cleanup: Array<() => void> = [];
afterEach(() => {
  for (const fn of cleanup.splice(0)) fn();
  vi.unstubAllGlobals();
});

const folderNodes: CockpitFileTreeNode[] = [
  { path: "src", name: "src", depth: 0, isDirectory: true },
  { path: "src/nested", name: "nested", depth: 1, isDirectory: true },
  { path: "src/nested/app.ts", name: "app.ts", depth: 2, isDirectory: false },
  { path: "src/main.ts", name: "main.ts", depth: 1, isDirectory: false },
  { path: "README.md", name: "README.md", depth: 0, isDirectory: false },
].map((node) => ({ ...node, validationState: "UnchangedBlue" }));

function setup() {
  const openFile = vi.fn();
  const panel = new FileTreePanel(document, {
    className: "test",
    withFilter: true,
    onOpenFile: openFile,
  });
  document.body.append(panel.element);
  cleanup.push(() => {
    panel.destroy();
    panel.element.remove();
  });
  panel.update(folderNodes, "");
  const paths = () =>
    Array.from(panel.element.querySelectorAll<HTMLElement>(".hilo-file-tree [data-path]")).map(
      (row) => row.dataset.path,
    );
  const toggle = (path: string) =>
    panel.element.querySelector<HTMLElement>(`.hilo-file-tree [data-path="${path}"]`)?.click();
  return { panel, paths, toggle, openFile };
}

describe("file tree panel", () => {
  it("starts collapsed, preserves manual expansion on refresh and collapses on project switch", () => {
    const { panel, paths, toggle } = setup();
    expect(paths()).toEqual(["src", "README.md"]);
    toggle("src");
    expect(paths()).toEqual(["src", "src/nested", "src/main.ts", "README.md"]);
    toggle("src/nested");
    expect(paths()).toContain("src/nested/app.ts");

    // A host refresh keeps the folders the user opened, but a new folder stays closed.
    const refreshed = [
      ...folderNodes,
      {
        path: "new",
        name: "new",
        depth: 0,
        isDirectory: true,
        validationState: "UnchangedBlue" as const,
      },
      {
        path: "new/file.ts",
        name: "file.ts",
        depth: 1,
        isDirectory: false,
        validationState: "UnchangedBlue" as const,
      },
    ];
    panel.update(refreshed, "src/nested/app.ts");
    expect(paths()).toContain("src/nested/app.ts");
    expect(paths()).toContain("new");
    expect(paths()).not.toContain("new/file.ts");
    panel.reset();
    panel.update([...folderNodes], "");
    expect(paths()).toEqual(["src", "README.md"]);
  });

  it("reveals filter matches temporarily without expanding the rest of the tree", () => {
    const { panel, paths, toggle, openFile } = setup();
    const filter = panel.element.querySelector<HTMLInputElement>("input");
    if (!filter) throw new Error("Missing filter");
    filter.value = "app.ts";
    filter.dispatchEvent(new Event("input"));
    expect(paths()).toEqual(["src", "src/nested", "src/nested/app.ts"]);
    toggle("src/nested/app.ts");
    expect(openFile).toHaveBeenCalledWith(folderNodes[2]);
    filter.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(paths()).toEqual(["src", "README.md"]);
  });

  it("draws the rows that fill its real height as soon as it is laid out, without a scroll", () => {
    let notify: () => void = () => {};
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(callback: () => void) {
          notify = callback;
        }
        observe(): void {}
        disconnect(): void {}
      },
    );
    const nodes: CockpitFileTreeNode[] = Array.from({ length: 200 }, (_, index) => ({
      path: `file-${String(index).padStart(3, "0")}.ts`,
      name: `file-${String(index).padStart(3, "0")}.ts`,
      depth: 0,
      isDirectory: false,
      validationState: "UnchangedBlue" as const,
    }));
    const panel = new FileTreePanel(document, {
      className: "test",
      withFilter: false,
      onOpenFile: () => {},
    });
    document.body.append(panel.element);
    panel.update(nodes, "");
    const rows = () => panel.element.querySelectorAll(".hilo-file-tree [data-path]").length;
    const before = rows();

    // The panel turns out to be much taller than the default the tree assumed.
    const scroller = panel.element.querySelector<HTMLElement>(".hilo-file-tree-scroll");
    Object.defineProperty(scroller, "clientHeight", { configurable: true, value: 3000 });
    notify();
    expect(rows()).toBeGreaterThan(before);
    panel.destroy();
    panel.element.remove();
  });
});
