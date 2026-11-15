// @vitest-environment happy-dom
import { ColorideEditorView, InMemoryBinaryHostBridge } from "@coloride/editor-cm6";
import { describe, expect, it, vi } from "vitest";
import { CockpitDomController, createCockpitStoreBundle } from "../index.js";
import { AgentRegistryPanel } from "./agent-registry-panel.js";
import {
  type AgentRegistryHost,
  type AgentRegistryListing,
  type AgentRegistryState,
  AgentRegistryStore,
  type InstalledAgent,
  type RegistryAgent,
  visibleRows,
} from "./agent-registry.js";
import { SettingsDialog } from "./settings-dialog.js";

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};

function agent(id: string, extra: Partial<RegistryAgent> = {}): RegistryAgent {
  return {
    id,
    name: id.toUpperCase(),
    version: "1.0.0",
    description: `The ${id} agent`,
    repository: null,
    website: null,
    license: "MIT",
    licenseUrl: null,
    distribution: "binary",
    blocked: null,
    ...extra,
  };
}

function installed(agentId: string, version = "1.0.0"): InstalledAgent {
  return {
    id: `${agentId}@${version}`,
    agentId,
    name: agentId.toUpperCase(),
    version,
    distribution: "binary",
  };
}

function listing(
  agents: RegistryAgent[],
  installedAgents: InstalledAgent[] = [],
  extra: Partial<AgentRegistryListing> = {},
): AgentRegistryListing {
  return {
    agents,
    installed: installedAgents,
    origin: "network",
    refreshError: null,
    ...extra,
  };
}

/** A Host whose answers the test controls. */
function fakeHost(initial: AgentRegistryListing) {
  let current = initial;
  const host: AgentRegistryHost = {
    list: vi.fn(async () => current),
    install: vi.fn(async (agentId: string, version: string) => {
      const done = installed(agentId, version);
      current = { ...current, installed: [...current.installed, done] };
      return done;
    }),
    uninstall: vi.fn(async (id: string) => {
      current = { ...current, installed: current.installed.filter((item) => item.id !== id) };
    }),
  };
  return {
    host,
    set: (next: AgentRegistryListing) => {
      current = next;
    },
  };
}

const buttons = (root: ParentNode, label: string) =>
  [...root.querySelectorAll("button")].filter((b) => b.getAttribute("aria-label") === label);
const click = (root: ParentNode, label: string) => {
  const found = buttons(root, label)[0];
  if (!found) throw new Error(`no button "${label}"`);
  found.click();
};
const cards = (root: ParentNode) => [
  ...root.querySelectorAll<HTMLElement>(".coloride-registry-card"),
];

describe("agent registry state", () => {
  it("loads the catalog and the installed agents", async () => {
    const { host } = fakeHost(listing([agent("a")], [installed("a")]));
    const store = new AgentRegistryStore(host);
    expect(store.getState().loading).toBe(true);
    await store.load();
    const state = store.getState();
    expect(state.loading).toBe(false);
    expect(state.agents.map((a) => a.id)).toEqual(["a"]);
    expect(state.installed.map((a) => a.agentId)).toEqual(["a"]);
    expect(host.list).toHaveBeenCalledWith(true);
  });

  it("reports a listing that cannot be loaded and keeps what it had", async () => {
    const { host } = fakeHost(listing([agent("a")]));
    const store = new AgentRegistryStore(host);
    await store.load();
    vi.mocked(host.list).mockRejectedValueOnce(new Error("The registry could not be reached"));
    await store.load();
    expect(store.getState().error).toBe("The registry could not be reached");
    expect(store.getState().agents).toHaveLength(1);
    await store.load();
    expect(store.getState().error).toBe("");
  });

  it("installs the exact version it is showing, then reloads from the Host without the network", async () => {
    const { host } = fakeHost(listing([agent("a", { version: "2.0.0" })]));
    const store = new AgentRegistryStore(host);
    await store.load();
    await store.install(store.getState().agents[0] as RegistryAgent);
    expect(host.install).toHaveBeenCalledWith("a", "2.0.0");
    expect(host.list).toHaveBeenLastCalledWith(false);
    expect(store.getState().installed[0]?.version).toBe("2.0.0");
    expect(store.getState().notice).toContain("installed");
    expect(store.getState().pending).toEqual({});
  });

  it("does nothing when the user declines the Host's confirmation", async () => {
    const { host } = fakeHost(listing([agent("a")]));
    vi.mocked(host.install).mockResolvedValueOnce(null);
    const store = new AgentRegistryStore(host);
    await store.load();
    await store.install(store.getState().agents[0] as RegistryAgent);
    expect(store.getState().notice).toBe("");
    expect(store.getState().failures).toEqual({});
    expect(store.getState().installed).toEqual([]);
    expect(host.list).toHaveBeenCalledTimes(1);
  });

  it("never asks the Host to install an agent that is blocked", async () => {
    const { host } = fakeHost(listing([agent("a", { blocked: "No checksum" })]));
    const store = new AgentRegistryStore(host);
    await store.load();
    await store.install(store.getState().agents[0] as RegistryAgent);
    expect(host.install).not.toHaveBeenCalled();
  });

  it("shows a failure on its own card and clears it on the next try", async () => {
    const { host } = fakeHost(listing([agent("a"), agent("b")]));
    vi.mocked(host.install).mockRejectedValueOnce(new Error("The download does not match"));
    const store = new AgentRegistryStore(host);
    await store.load();
    const [a, b] = store.getState().agents as [RegistryAgent, RegistryAgent];
    await store.install(a);
    expect(store.getState().failures).toEqual({ a: "The download does not match" });
    expect(store.getState().pending).toEqual({});
    await store.install(b);
    expect(store.getState().failures).toEqual({ a: "The download does not match" });
    await store.install(a);
    expect(store.getState().failures).toEqual({});
  });

  it("ignores a second click while an install is running", async () => {
    const { host } = fakeHost(listing([agent("a")]));
    let finish: (value: InstalledAgent | null) => void = () => undefined;
    vi.mocked(host.install).mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const store = new AgentRegistryStore(host);
    await store.load();
    const a = store.getState().agents[0] as RegistryAgent;
    const first = store.install(a);
    expect(store.getState().pending).toEqual({ a: "installing" });
    await store.install(a);
    finish(installed("a"));
    await first;
    expect(host.install).toHaveBeenCalledTimes(1);
  });

  it("removes an installation by its id", async () => {
    const { host } = fakeHost(listing([agent("a")], [installed("a")]));
    const store = new AgentRegistryStore(host);
    await store.load();
    await store.uninstall(store.getState().installed[0] as InstalledAgent);
    expect(host.uninstall).toHaveBeenCalledWith("a@1.0.0");
    expect(store.getState().installed).toEqual([]);
    expect(store.getState().notice).toContain("removed");
  });

  it("lets only the latest listing write its result", async () => {
    const { host } = fakeHost(listing([]));
    const resolvers: ((value: AgentRegistryListing) => void)[] = [];
    vi.mocked(host.list).mockImplementation(
      () => new Promise((resolve) => resolvers.push(resolve)),
    );
    const store = new AgentRegistryStore(host);
    const slow = store.load();
    const fast = store.load();
    resolvers[1]?.(listing([agent("new")]));
    await fast;
    resolvers[0]?.(listing([agent("old")]));
    await slow;
    expect(store.getState().agents.map((a) => a.id)).toEqual(["new"]);
  });

  it("writes nothing once disposed", async () => {
    const { host } = fakeHost(listing([agent("a")]));
    const store = new AgentRegistryStore(host);
    const loading = store.load();
    store.dispose();
    await loading;
    expect(store.getState().agents).toEqual([]);
  });
});

describe("agent registry rows", () => {
  const base = {
    loading: false,
    origin: "network" as const,
    refreshError: null,
    query: "",
    filter: "all" as const,
    pending: {},
    failures: {},
    error: "",
    notice: "",
  };

  it("filters by installation, searches name, id and description, and sorts by name", () => {
    const state = {
      ...base,
      agents: [
        agent("zeta", { name: "Zeta" }),
        agent("alpha", { name: "Alpha", description: "writes Rust" }),
        agent("mid", { name: "Mid" }),
      ],
      installed: [installed("mid")],
    };
    const names = (change: Partial<AgentRegistryState>) =>
      visibleRows({ ...state, ...change }).map((row) => row.agent.name);
    expect(names({})).toEqual(["Alpha", "Mid", "Zeta"]);
    expect(names({ filter: "installed" })).toEqual(["Mid"]);
    expect(names({ filter: "available" })).toEqual(["Alpha", "Zeta"]);
    expect(names({ query: "  RUST " })).toEqual(["Alpha"]);
    expect(names({ query: "zeta" })).toEqual(["Zeta"]);
    expect(names({ query: "nothing" })).toEqual([]);
  });

  it("marks an installed agent as updatable when another version is on offer", () => {
    const rows = visibleRows({
      ...base,
      agents: [agent("a", { version: "2.0.0" }), agent("b")],
      installed: [installed("a", "1.0.0"), installed("b", "1.0.0")],
    });
    expect(rows.map((row) => row.updatable)).toEqual([true, false]);
  });

  it("keeps an installed agent the catalog dropped, so it can still be removed", () => {
    const rows = visibleRows({ ...base, agents: [], installed: [installed("gone")] });
    expect(rows).toHaveLength(1);
    expect(rows[0]?.installed?.agentId).toBe("gone");
    expect(rows[0]?.agent.blocked).toContain("No longer");
  });
});

describe("agent registry panel", () => {
  async function mount(initial: AgentRegistryListing) {
    const fake = fakeHost(initial);
    const panel = new AgentRegistryPanel(document, fake.host);
    document.body.append(panel.element);
    await settle();
    return { ...fake, panel };
  }

  it("shows one card per agent with its version, license and install button", async () => {
    const { panel } = await mount(listing([agent("a"), agent("b", { version: "3.1.0" })]));
    expect(cards(panel.element).map((card) => card.getAttribute("aria-label"))).toEqual(["A", "B"]);
    expect(cards(panel.element)[1]?.textContent).toContain("v3.1.0");
    expect(cards(panel.element)[1]?.textContent).toContain("MIT");
    expect(buttons(panel.element, "Install B 3.1.0")).toHaveLength(1);
    panel.destroy();
  });

  it("installs from the card and then offers to remove it", async () => {
    const { panel, host } = await mount(listing([agent("a")]));
    click(panel.element, "Install A 1.0.0");
    await settle();
    expect(host.install).toHaveBeenCalledWith("a", "1.0.0");
    expect(buttons(panel.element, "Install A 1.0.0")).toHaveLength(0);
    click(panel.element, "Remove A");
    await settle();
    expect(host.uninstall).toHaveBeenCalledWith("a@1.0.0");
    expect(buttons(panel.element, "Install A 1.0.0")).toHaveLength(1);
    panel.destroy();
  });

  it("says why an agent cannot be installed and disables its button", async () => {
    const { panel } = await mount(
      listing([agent("c", { blocked: "The registry publishes no SHA-256 for this download." })]),
    );
    const card = cards(panel.element)[0] as HTMLElement;
    expect(card.textContent).toContain("Cannot be installed here");
    expect(card.textContent).toContain("SHA-256");
    expect(buttons(panel.element, "Install C 1.0.0")[0]?.disabled).toBe(true);
    panel.destroy();
  });

  it("offers an update beside Remove when another version is on offer", async () => {
    const { panel, host } = await mount(
      listing([agent("a", { version: "2.0.0" })], [installed("a", "1.0.0")]),
    );
    const card = cards(panel.element)[0] as HTMLElement;
    expect(card.textContent).toContain("v1.0.0");
    expect(card.textContent).toContain("Installed 1.0.0; 2.0.0 is available.");
    click(panel.element, "Update A to 2.0.0");
    await settle();
    expect(host.install).toHaveBeenCalledWith("a", "2.0.0");
    panel.destroy();
  });

  it("filters and searches without reloading", async () => {
    const { panel, host } = await mount(
      listing([agent("alpha"), agent("beta")], [installed("beta")]),
    );
    const filter = (label: string) =>
      [...panel.element.querySelectorAll(".coloride-registry-filters button")].find(
        (b) => b.textContent === label,
      ) as HTMLButtonElement;
    filter("Installed").click();
    expect(cards(panel.element).map((c) => c.dataset.agent)).toEqual(["beta"]);
    expect(filter("Installed").getAttribute("aria-pressed")).toBe("true");
    filter("All").click();
    const search = panel.element.querySelector("input") as HTMLInputElement;
    search.value = "alp";
    search.dispatchEvent(new Event("input"));
    expect(cards(panel.element).map((c) => c.dataset.agent)).toEqual(["alpha"]);
    search.value = "zzz";
    search.dispatchEvent(new Event("input"));
    expect(panel.element.querySelector(".coloride-registry-empty")?.textContent).toContain(
      "No agents match",
    );
    expect(host.list).toHaveBeenCalledTimes(1);
    panel.destroy();
  });

  it("says when the saved catalog is shown and retries from the network", async () => {
    const { panel, host, set } = await mount(
      listing([agent("a")], [], { origin: "saved", refreshError: "Download failed: offline" }),
    );
    const alert = panel.element.querySelector(".coloride-registry-alert") as HTMLElement;
    expect(alert.hidden).toBe(false);
    expect(alert.textContent).toContain("saved earlier");
    expect(alert.textContent).toContain("Download failed: offline");
    set(listing([agent("a"), agent("b")]));
    (alert.querySelector("button") as HTMLButtonElement).click();
    await settle();
    expect(host.list).toHaveBeenLastCalledWith(true);
    expect(cards(panel.element)).toHaveLength(2);
    expect((panel.element.querySelector(".coloride-registry-alert") as HTMLElement).hidden).toBe(
      true,
    );
    panel.destroy();
  });

  it("shows a failed install on its card as an alert", async () => {
    const { panel, host } = await mount(listing([agent("a")]));
    vi.mocked(host.install).mockRejectedValueOnce(new Error("npm could not install a@1.0.0: 404"));
    click(panel.element, "Install A 1.0.0");
    await settle();
    const failure = panel.element.querySelector(".coloride-registry-failure") as HTMLElement;
    expect(failure.getAttribute("role")).toBe("alert");
    expect(failure.textContent).toContain("404");
    expect(buttons(panel.element, "Install A 1.0.0")[0]?.disabled).toBe(false);
    panel.destroy();
  });

  it("keeps the keyboard focus on the same button when the cards are rebuilt", async () => {
    const { panel } = await mount(listing([agent("a"), agent("b")]));
    const target = buttons(panel.element, "Install B 1.0.0")[0] as HTMLButtonElement;
    target.focus();
    expect(document.activeElement).toBe(target);
    panel.store.setQuery("");
    panel.store.setFilter("all");
    await settle();
    const after = buttons(panel.element, "Install B 1.0.0")[0];
    expect(after).not.toBe(target);
    expect(document.activeElement).toBe(after);
    panel.destroy();
  });

  it("renders hostile catalog text as text, never as markup", async () => {
    const { panel } = await mount(
      listing([
        agent("x", {
          name: "<img src=x onerror=alert(1)>",
          description: "<script>attack()</script>",
          blocked: "<b>bold</b>",
        }),
      ]),
    );
    expect(panel.element.querySelector("img, script, b")).toBeNull();
    expect(cards(panel.element)[0]?.textContent).toContain("<script>attack()</script>");
    panel.destroy();
  });
});

describe("settings dialog", () => {
  function open(initial = listing([agent("a")])) {
    const fake = fakeHost(initial);
    const parent = document.createElement("div");
    document.body.append(parent);
    const trigger = document.createElement("button");
    parent.append(trigger);
    trigger.focus();
    const dialog = new SettingsDialog(document, parent, { agentRegistry: fake.host });
    return { ...fake, parent, trigger, dialog };
  }

  it("opens as a labelled modal dialog on the registry and focuses its search", async () => {
    const { dialog, parent } = open();
    dialog.open();
    await settle();
    const shown = parent.querySelector('[role="dialog"]') as HTMLElement;
    expect(shown.getAttribute("aria-modal")).toBe("true");
    expect(shown.getAttribute("aria-labelledby")).toBe("coloride-settings-title");
    expect(parent.querySelector("#coloride-settings-title")?.textContent).toBe("Settings");
    expect(parent.querySelector(".coloride-registry")).not.toBeNull();
    expect(document.activeElement?.getAttribute("aria-label")).toBe("Search agents");
    dialog.close();
  });

  it("closes with Escape, the close button or a click outside, and returns the focus", async () => {
    for (const how of ["escape", "button", "backdrop"] as const) {
      const { dialog, parent, trigger } = open();
      dialog.open();
      await settle();
      const overlay = parent.querySelector(".coloride-settings-overlay") as HTMLElement;
      if (how === "escape")
        overlay.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      else if (how === "button")
        (parent.querySelector(".coloride-settings-close") as HTMLButtonElement).click();
      else overlay.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      expect(dialog.isOpen, how).toBe(false);
      expect(parent.querySelector(".coloride-settings-overlay"), how).toBeNull();
      expect(document.activeElement, how).toBe(trigger);
    }
  });

  it("keeps Tab inside the dialog", async () => {
    const { dialog, parent } = open();
    dialog.open();
    await settle();
    const shown = parent.querySelector('[role="dialog"]') as HTMLElement;
    const focusable = [
      ...shown.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled])"),
    ];
    const first = focusable[0] as HTMLElement;
    const last = focusable[focusable.length - 1] as HTMLElement;
    last.focus();
    const forward = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    last.dispatchEvent(forward);
    expect(forward.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(first);
    const back = new KeyboardEvent("keydown", {
      key: "Tab",
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    first.dispatchEvent(back);
    expect(back.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(last);
    dialog.close();
  });

  it("reads the registry afresh each time it is opened", async () => {
    const { dialog, host } = open();
    dialog.open();
    await settle();
    dialog.close();
    dialog.open();
    await settle();
    expect(host.list).toHaveBeenCalledTimes(2);
    expect(host.list).toHaveBeenCalledWith(true);
    dialog.toggle();
    expect(dialog.isOpen).toBe(false);
  });
});

describe("settings in the cockpit", () => {
  function mount(registry?: AgentRegistryHost) {
    const root = document.createElement("div");
    document.body.append(root);
    const stores = createCockpitStoreBundle();
    const editor = new ColorideEditorView({
      initialDocumentId: "src/file.ts",
      initialContent: "const a = 1;",
      ipcChannel: new InMemoryBinaryHostBridge(),
    });
    const cockpit = new CockpitDomController({
      rootContainer: root,
      stores,
      editorView: editor,
      ...(registry ? { agentRegistryHost: registry } : {}),
    });
    return {
      root,
      stores,
      editor,
      cockpit,
      cleanup: () => {
        cockpit.destroy();
        editor.destroy();
        root.remove();
      },
    };
  }

  it("offers no Settings button or shortcut without a Host registry", () => {
    const { root, cleanup } = mount();
    expect(root.querySelector(".coloride-settings-btn")).toBeNull();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: ",", metaKey: true }));
    expect(root.querySelector(".coloride-settings-overlay")).toBeNull();
    cleanup();
  });

  it("opens from the activity bar and from Cmd/Ctrl+, without remounting the editor", async () => {
    const { host } = fakeHost(listing([agent("a")]));
    const { root, stores, editor, cleanup } = mount(host);
    const instance = editor.getEditorView();
    const parent = instance.dom.parentElement;
    const zone4Updates = stores.zone4.getUpdateCount();

    (root.querySelector(".coloride-settings-btn") as HTMLButtonElement).click();
    await settle();
    expect(root.querySelector(".coloride-settings-overlay")).not.toBeNull();
    expect(cards(root).map((c) => c.dataset.agent)).toEqual(["a"]);
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: ",", ctrlKey: true, bubbles: true }),
    );
    expect(root.querySelector(".coloride-settings-overlay")).toBeNull();
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: ",", metaKey: true, bubbles: true }),
    );
    expect(root.querySelector(".coloride-settings-overlay")).not.toBeNull();

    expect(editor.getEditorView()).toBe(instance);
    expect(instance.dom.parentElement).toBe(parent);
    expect(editor.getStoreSnapshot().mountCount).toBe(1);
    expect(stores.zone4.getUpdateCount()).toBe(zone4Updates);
    cleanup();
    expect(document.querySelector(".coloride-settings-overlay")).toBeNull();
  });
});
