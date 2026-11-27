// @vitest-environment happy-dom
import { HiloEditorView, InMemoryBinaryHostBridge } from "@hilo/editor-cm6";
import { afterEach, describe, expect, it } from "vitest";
import { CockpitDomController, createCockpitStoreBundle } from "../../index.js";
import type { CockpitSearchResultItem } from "../../stores/types.js";

const cleanups: Array<() => void> = [];
afterEach(() => {
  for (const cleanup of cleanups.splice(0)) cleanup();
});

const hit = (filePath: string): CockpitSearchResultItem => ({
  filePath,
  line: 1,
  snippet: filePath,
  freshness: "IndexedCommit",
  score: 1,
  secretsRedacted: 0,
});

describe("Workspace search", () => {
  it("shows the results of the latest search, even when an earlier one answers last", async () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const pending = new Map<string, (results: CockpitSearchResultItem[]) => void>();
    const stores = createCockpitStoreBundle();
    const editorView = new HiloEditorView({
      initialDocumentId: "src/main.ts",
      initialContent: "",
      ipcChannel: new InMemoryBinaryHostBridge(),
    });
    const controller = new CockpitDomController({
      rootContainer: root,
      stores,
      editorView,
      searchWorkspace: (query) =>
        new Promise((resolve) => {
          pending.set(query, resolve);
        }),
    });
    cleanups.push(() => {
      controller.destroy();
      editorView.destroy();
      root.remove();
    });
    const input = root.querySelector<HTMLInputElement>(".hilo-global-search-input");
    if (!input) throw new Error("search input missing");
    const search = (query: string) => {
      input.value = query;
      input.dispatchEvent(new Event("change"));
    };

    search("old");
    search("new");
    pending.get("new")?.([hit("src/new.ts")]);
    await Promise.resolve();
    pending.get("old")?.([hit("src/old.ts")]);
    await Promise.resolve();

    const shown = stores.zone2.getState();
    expect(shown.searchQuery).toBe("new");
    expect(shown.searchResults.map((result) => result.filePath)).toEqual(["src/new.ts"]);
  });
});
