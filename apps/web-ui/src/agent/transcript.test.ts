// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import { AgentPanel } from "./panel.js";
import { type AgentHost, type AgentState, AgentStore } from "./store.js";
import { parseTranscript, serializeTranscript } from "./transcript.js";

const event = (update: Record<string, unknown>) => ({ type: "update" as const, update });
const chunk = (text: string) =>
  event({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
function conversation(): AgentStore {
  const store = new AgentStore((callback) => callback());
  store.enqueue({ type: "message", role: "user", text: "analyze" });
  store.enqueue(chunk("First "));
  store.enqueue(chunk("part."));
  store.enqueue(
    event({
      sessionUpdate: "tool_call",
      toolCallId: "t1",
      kind: "execute",
      title: "ls",
      status: "completed",
      rawInput: { command: "ls -la" },
      content: [{ type: "content", content: { type: "text", text: "a.txt" } }],
    }),
  );
  store.enqueue(
    event({
      sessionUpdate: "tool_call",
      toolCallId: "t2",
      kind: "read",
      title: "Read",
      status: "in_progress",
    }),
  );
  store.enqueue(chunk("Second."));
  return store;
}

describe("saved conversations", () => {
  it("round-trips a conversation in order, with tools and text intact", () => {
    const state = conversation().getState();
    const json = serializeTranscript(state, "codex");
    expect(json).not.toBeNull();
    const snapshot = parseTranscript(json as string, "codex");
    expect(snapshot?.timeline.map((ref) => ref.type)).toEqual([
      "message",
      "message",
      "tool",
      "tool",
      "message",
    ]);
    expect(snapshot?.messages.map((m) => m.text)).toEqual(["analyze", "First part.", "Second."]);
    expect(snapshot?.tools[0]).toMatchObject({
      command: "ls -la",
      output: "a.txt",
      status: "done",
    });
    expect(json).not.toContain("<p>");
  });
  it("keeps nothing for an empty panel, and nothing of another agent", () => {
    expect(serializeTranscript(new AgentStore().getState(), "codex")).toBeNull();
    const json = serializeTranscript(conversation().getState(), "codex") as string;
    expect(parseTranscript(json, "claude")).toBeNull();
  });
  it("refuses damaged or hostile files, and drops what is not valid", () => {
    expect(parseTranscript("not json", "codex")).toBeNull();
    expect(parseTranscript(JSON.stringify({ version: 9, agentId: "codex" }), "codex")).toBeNull();
    const hostile = {
      version: 1,
      agentId: "codex",
      messages: [
        { id: "message-0", role: "assistant", text: "<img src=x onerror=alert(1)>" },
        { id: "message-1", role: "system", text: "bad role" },
        { id: 5, role: "user", text: "bad id" },
      ],
      tools: [
        {
          id: "a",
          kind: "read",
          title: "ok",
          status: "done",
          locations: [
            { path: "/etc/passwd", line: 1 },
            { path: "../x", line: 1 },
            { path: "src/a.ts", line: 3 },
          ],
        },
        { id: "b", kind: "read", title: "bad status", status: "weird" },
      ],
      timeline: [
        { type: "message", id: "message-0" },
        { type: "message", id: "message-1" },
        { type: "tool", id: "a" },
        { type: "tool", id: "b" },
        { type: "tool", id: "missing" },
      ],
    };
    const snapshot = parseTranscript(JSON.stringify(hostile), "codex");
    expect(snapshot?.messages.map((m) => m.id)).toEqual(["message-0"]);
    expect(snapshot?.tools.map((t) => t.id)).toEqual(["a"]);
    expect(snapshot?.tools[0]?.locations).toEqual([{ path: "src/a.ts", line: 3 }]);
    expect(snapshot?.timeline).toEqual([
      { type: "message", id: "message-0" },
      { type: "tool", id: "a" },
    ]);
    const store = new AgentStore(() => {});
    store.restore(snapshot as NonNullable<typeof snapshot>);
    expect(store.getState().messages[0]?.html.join("")).not.toContain("<img");
  });
  it("shows a restored conversation with unfinished tools marked as cut off", () => {
    const json = serializeTranscript(conversation().getState(), "codex") as string;
    const store = new AgentStore((callback) => callback());
    store.restore(
      parseTranscript(json, "codex") as NonNullable<ReturnType<typeof parseTranscript>>,
    );
    const state: AgentState = store.getState();
    expect(state.restored).toBe(true);
    expect(state.tools.find((t) => t.id === "t2")?.status).toBe("denied");
    expect(state.tools.find((t) => t.id === "t1")?.status).toBe("done");
    // New text after a restore opens a new message, with a fresh id.
    store.enqueue(chunk("later"));
    expect(new Set(store.getState().messages.map((m) => m.id)).size).toBe(4);
  });
});

describe("a panel and its saved conversation", () => {
  function setup(saved: string | null) {
    const transcripts = {
      load: vi.fn(async (_chatId: string) => saved),
      save: vi.fn(async (_chatId: string, _json: string) => undefined),
      remove: vi.fn(async (_chatId: string) => undefined),
    };
    const host: AgentHost = {
      transcripts,
      start: vi.fn(async () => ({
        prompt: vi.fn(),
        cancel: vi.fn(),
        stop: vi.fn(async () => undefined),
        answer: vi.fn(),
      })),
    };
    return { transcripts, host };
  }
  const settle = async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
  };
  it("loads the conversation of a restored panel and keeps it when connecting", async () => {
    const json = serializeTranscript(conversation().getState(), "codex") as string;
    const { host, transcripts } = setup(json);
    const panel = new AgentPanel(
      document,
      host,
      async () => undefined,
      { agentId: "codex", name: "Codex" },
      false,
      new AgentStore((c) => c()),
      { chatId: "chat-1", restore: true },
    );
    await settle();
    expect(transcripts.load).toHaveBeenCalledWith("chat-1");
    expect(panel.store.getState().messages).toHaveLength(3);
    expect(panel.element.textContent).toContain("does not remember it");
    panel.activate();
    await settle();
    expect(panel.store.getState().messages).toHaveLength(3);
    panel.destroy();
  });
  it("does not load anything for a new panel, and saves what it shows", async () => {
    vi.useFakeTimers();
    const { host, transcripts } = setup("{}");
    const store = new AgentStore((c) => c());
    const panel = new AgentPanel(
      document,
      host,
      async () => undefined,
      { agentId: "codex", name: "Codex" },
      false,
      store,
      { chatId: "chat-2", restore: false },
    );
    expect(transcripts.load).not.toHaveBeenCalled();
    store.enqueue(chunk("hello"));
    expect(transcripts.save).not.toHaveBeenCalled();
    vi.advanceTimersByTime(700);
    expect(transcripts.save).toHaveBeenCalledTimes(1);
    expect(transcripts.save.mock.calls[0]?.[0]).toBe("chat-2");
    vi.useRealTimers();
    panel.discard();
    expect(transcripts.remove).toHaveBeenCalledWith("chat-2");
    store.enqueue(chunk(" more"));
    panel.destroy();
    expect(transcripts.save).toHaveBeenCalledTimes(1);
  });
  it("ignores a saved file that is not valid, and one of another agent", async () => {
    const { host } = setup("garbage");
    const panel = new AgentPanel(
      document,
      host,
      async () => undefined,
      { agentId: "codex", name: "Codex" },
      false,
      new AgentStore((c) => c()),
      { chatId: "chat-3", restore: true },
    );
    await settle();
    expect(panel.store.getState().restored).toBe(false);
    panel.destroy();
  });
});
