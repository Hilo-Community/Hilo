// @vitest-environment happy-dom
import { HiloEditorView, InMemoryBinaryHostBridge } from "@hilo/editor-cm6";
import { afterEach, describe, expect, it } from "vitest";
import {
  CockpitDomController,
  type TerminalHost,
  type WorkspaceTrustReport,
  createCockpitStoreBundle,
} from "../../index.js";

const cleanups: Array<() => void> = [];
afterEach(() => {
  for (const cleanup of cleanups.splice(0)) cleanup();
});
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

/** A host whose decision per folder only changes when its (native) prompt is answered. */
function setup(answer: "trust" | "decline" = "trust") {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const stores = createCockpitStoreBundle();
  const trusted = new Set<string>();
  let openRoot: string | null = null;
  let prompts = 0;
  const opened: boolean[] = [];
  const report = (): WorkspaceTrustReport => ({
    state: openRoot !== null && trusted.has(openRoot) ? "Trusted" : "Untrusted",
    workspaceRoot: openRoot,
  });
  const editorView = new HiloEditorView({
    initialDocumentId: "src/main.ts",
    initialContent: "const ready = true;\n",
    ipcChannel: new InMemoryBinaryHostBridge(),
  });
  // Records whether the folder was trusted at the moment a shell was asked for.
  const terminalHost: TerminalHost = {
    open: async () => {
      opened.push(report().state === "Trusted");
      return { write: () => undefined, resize: () => undefined, close: () => undefined };
    },
  };
  const controller = new CockpitDomController({
    rootContainer: root,
    stores,
    editorView,
    terminalHost,
    createTerminalView: () => ({
      cols: 80,
      rows: 24,
      open: () => undefined,
      write: () => undefined,
      onData: () => undefined,
      fit: () => false,
      focus: () => undefined,
      dispose: () => undefined,
    }),
    workspaceTrust: {
      getTrust: async () => report(),
      requestTrust: async () => {
        prompts += 1;
        if (answer === "trust" && openRoot !== null) trusted.add(openRoot);
        return report();
      },
    },
  });
  cleanups.push(() => {
    controller.destroy();
    editorView.destroy();
    root.remove();
  });
  const openFolder = async (path: string) => {
    openRoot = path;
    stores.setProjectPath(path);
    await settle();
  };
  const startTerminal = async () => {
    stores.createChat({ kind: "terminal" });
    await settle();
  };
  return {
    controller,
    editorView,
    root,
    openFolder,
    startTerminal,
    prompts: () => prompts,
    opened: () => opened,
  };
}

describe("workspace trust — asked for when a folder is used, not announced", () => {
  it("shows no notice at all, however the folder is restricted", async () => {
    const { root, controller, openFolder } = setup();
    await settle();
    expect(controller.getWorkspaceTrust()).toEqual({ state: "Untrusted", workspaceRoot: null });
    await openFolder("/work/unknown-repo");
    expect(root.querySelector(".hilo-trust-banner")).toBeNull();
    expect(controller.getWorkspaceTrust()?.state).toBe("Untrusted");
  });

  it("starting a terminal in an untrusted folder asks the host first, then starts it trusted", async () => {
    const { controller, openFolder, startTerminal, prompts, opened } = setup();
    await openFolder("/work/unknown-repo");
    expect(prompts()).toBe(0);

    await startTerminal();
    expect(prompts()).toBe(1);
    expect(opened()).toEqual([true]);
    expect(controller.getWorkspaceTrust()?.state).toBe("Trusted");
  });

  it("declining still lets the host refuse (and say why), and asks again next time", async () => {
    const { controller, openFolder, startTerminal, prompts, opened } = setup("decline");
    await openFolder("/work/unknown-repo");
    await startTerminal();
    expect(opened()).toEqual([false]);
    expect(controller.getWorkspaceTrust()?.state).toBe("Untrusted");
    await startTerminal();
    expect(prompts()).toBe(2);
  });

  it("a trusted folder is not asked again, and trust does not carry over to another folder", async () => {
    const { openFolder, startTerminal, prompts } = setup();
    await openFolder("/work/mine");
    await startTerminal();
    await startTerminal();
    expect(prompts()).toBe(1);
    await openFolder("/work/downloaded");
    await startTerminal();
    expect(prompts()).toBe(2);
    await openFolder("/work/mine");
    await startTerminal();
    expect(prompts()).toBe(2);
  });

  it("editing and Lezer highlighting work normally in an untrusted folder", async () => {
    const { controller, editorView, openFolder } = setup("decline");
    await openFolder("/work/unknown-repo");
    expect(controller.getWorkspaceTrust()?.state).toBe("Untrusted");

    editorView.dispatchUserEdit(0, 0, "export ");
    expect(editorView.getDocumentText()).toBe("export const ready = true;\n");
    // Lezer runs in the webview, with no process involved: keywords are still tokenized.
    const content = editorView.getEditorView().contentDOM;
    const highlighted = Array.from(content.querySelectorAll("span")).filter(
      (span) => span.className !== "" && /^(export|const|true)$/.test(span.textContent ?? ""),
    );
    expect(highlighted.length).toBeGreaterThan(0);
  });
});
