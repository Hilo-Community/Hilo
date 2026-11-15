// @vitest-environment happy-dom
import { ColorideEditorView, InMemoryBinaryHostBridge } from "@coloride/editor-cm6";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CockpitDomController, createCockpitStoreBundle } from "../index.js";
import { AgentPanel } from "./panel.js";
import { type AcpEvent, type AgentHandle, type AgentHost, AgentStore } from "./store.js";

afterEach(() => vi.unstubAllGlobals());
const chunk = (text: string): AcpEvent => ({
  type: "update",
  update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } },
});
const button = (root: HTMLElement, label: string) =>
  [...root.querySelectorAll("button")].find((b) => b.textContent === label) as HTMLButtonElement;
const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};
function host() {
  let receive: (event: AcpEvent) => void = () => undefined;
  const handle: AgentHandle = {
    prompt: vi.fn(async () => undefined),
    cancel: vi.fn(async () => undefined),
    stop: vi.fn(async () => undefined),
    answer: vi.fn(async () => undefined),
  };
  const host: AgentHost = {
    start: vi.fn(async (_, onEvent) => {
      receive = onEvent;
      return handle;
    }),
  };
  return { host, handle, emit: (event: AcpEvent) => receive(event) };
}
describe("LOT-014 agent panel", () => {
  it("batches tokens into one independent atomic update and escapes hostile Markdown", () => {
    const frames: (() => void)[] = [];
    const store = new AgentStore((cb) => frames.push(cb));
    for (let i = 0; i < 150; i++) store.enqueue(chunk(i === 0 ? "<script>attack()</script>" : "x"));
    expect(store.getUpdateCount()).toBe(0);
    expect(frames).toHaveLength(1);
    frames[0]?.();
    expect(store.getUpdateCount()).toBe(1);
    expect(store.getState().messages).toHaveLength(1);
    expect(store.getState().messages[0]?.html.join("")).toContain("&lt;script&gt;");
    store.dispose();
    store.enqueue(chunk("late"));
    expect(store.getUpdateCount()).toBe(1);
  });
  it("merges plans, tools and proposals, filters unsafe locations, clears permissions on stop", () => {
    const store = new AgentStore(() => undefined);
    store.enqueue({
      type: "update",
      update: { sessionUpdate: "plan", entries: [{ content: "Test", status: "completed" }] },
    });
    store.enqueue({
      type: "update",
      update: {
        sessionUpdate: "tool_call",
        toolCallId: "t",
        title: "Read",
        kind: "read",
        status: "pending",
        locations: [{ path: "../.env" }, { path: "src/file.ts", line: 8 }],
      },
    });
    store.enqueue({
      type: "update",
      update: { sessionUpdate: "tool_call_update", toolCallId: "t", status: "completed" },
    });
    store.enqueue({ type: "permission", id: "p", reason: "Approval required", options: [] });
    store.flush();
    expect(store.getState().tools[0]).toMatchObject({
      title: "Read",
      status: "done",
      locations: [{ path: "src/file.ts", line: 8 }],
    });
    expect(store.getState().plan[0]?.status).toBe("completed");
    store.enqueue({ type: "stopped", reason: "Cancelled" });
    store.flush();
    expect(store.getState().permissions).toEqual([]);
    expect(store.getState().ready).toBe(false);
  });
  it("sends prompts, shows a read-only diff, opens a location, answers once and cancels", async () => {
    const fake = host();
    const open = vi.fn(async () => undefined);
    const store = new AgentStore(() => undefined);
    const panel = new AgentPanel(document, fake.host, open, store);
    document.body.append(panel.element);
    button(panel.element, "Start agent").click();
    await settle();
    fake.emit({ type: "ready", sessionId: "s" });
    store.flush();
    const prompt = panel.element.querySelector("textarea") as HTMLTextAreaElement;
    prompt.value = "Repair tests";
    panel.element.querySelector("form")?.dispatchEvent(new Event("submit", { cancelable: true }));
    await settle();
    expect(fake.handle.prompt).toHaveBeenCalledWith("Repair tests");
    fake.emit({
      type: "proposedEdit",
      edit: {
        id: "e",
        path: "src/file.ts",
        baseContentHash: "abc",
        oldText: "old 🦀",
        newText: "new 🦀",
      },
    });
    fake.emit({ type: "permission", id: "p", reason: "Review exact call", options: [] });
    store.flush();
    expect(panel.element.querySelector("pre")?.textContent).toContain("- old 🦀\n+ new 🦀");
    expect(panel.element.querySelector("[contenteditable]")).toBeNull();
    button(panel.element, "src/file.ts:1").click();
    await settle();
    expect(open).toHaveBeenCalledWith("src/file.ts", 1);
    button(panel.element, "Allow once").click();
    await settle();
    expect(fake.handle.answer).toHaveBeenCalledWith("p", true);
    button(panel.element, "Cancel").click();
    await settle();
    expect(fake.handle.cancel).toHaveBeenCalledOnce();
    panel.destroy();
    expect(fake.handle.stop).toHaveBeenCalledOnce();
  });
  it("streaming in the mounted cockpit preserves the exact EditorView and mount container", async () => {
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      frames.push(cb);
      return frames.length;
    });
    const fake = host();
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
      agentHost: fake.host,
    });
    const instance = editor.getEditorView();
    const parent = instance.dom.parentElement;
    const zone4Updates = stores.zone4.getUpdateCount();
    const start = button(root, "Start agent");
    expect(start).toBeDefined();
    start.click();
    await settle();
    for (let i = 0; i < 150; i++) fake.emit(chunk("token "));
    for (const frame of frames.splice(0)) frame(16);
    expect(root.querySelector(".coloride-agent-output")?.textContent).toContain("token");
    expect(editor.getEditorView()).toBe(instance);
    expect(instance.dom.parentElement).toBe(parent);
    expect(editor.getStoreSnapshot().mountCount).toBe(1);
    expect(stores.zone4.getUpdateCount()).toBe(zone4Updates);
    cockpit.destroy();
    editor.destroy();
    root.remove();
  });
});
