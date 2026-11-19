// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import { ConversationView } from "./conversation-view.js";
import { formatDuration } from "./duration.js";
import { OptionBar } from "./option-bar.js";
import { AgentPanel } from "./panel.js";
import { type AgentHost, type AgentOption, AgentStore } from "./store.js";

const model: AgentOption = {
  id: "model",
  name: "Model",
  category: "model",
  current: "deep",
  choices: [
    { value: "deep", name: "Deep" },
    { value: "fast", name: "Fast" },
  ],
};
const mode: AgentOption = {
  id: "mode",
  name: "Mode",
  category: "mode",
  current: "default",
  choices: [
    { value: "default", name: "Default" },
    { value: "plan", name: "Plan" },
  ],
};
const button = () => document.createElement("button");
const actions = { openLocation: () => {}, answer: () => {} };

describe("durations", () => {
  it("reads as people say it", () => {
    expect(formatDuration(400)).toBe("0.4s");
    expect(formatDuration(8000)).toBe("8.0s");
    expect(formatDuration(14_400)).toBe("14s");
    expect(formatDuration(65_000)).toBe("1m 05s");
    expect(formatDuration(-5)).toBe("0.0s");
  });
});

describe("where a turn's time goes", () => {
  function clocked() {
    let now = 0;
    const store = new AgentStore(
      (callback) => callback(),
      () => now,
    );
    const at = (t: number) => {
      now = t;
    };
    return { store, at };
  }
  it("times each tool, and shows it only when it took a while", () => {
    const { store, at } = clocked();
    const call = (id: string, status: string) => ({
      type: "update" as const,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: id,
        kind: "execute",
        title: id,
        status,
      },
    });
    at(1000);
    store.enqueue(call("slow", "in_progress"));
    store.enqueue(call("quick", "in_progress"));
    at(3500);
    store.enqueue(call("slow", "completed"));
    at(1200 + 1000);
    store.enqueue(call("quick", "completed"));
    const tools = store.getState().tools;
    expect(tools.find((t) => t.id === "slow")?.durationMs).toBe(2500);
    expect(tools.find((t) => t.id === "quick")?.durationMs).toBe(1200);
    const shown = new ConversationView().elements(document, store.getState(), button, actions);
    expect(shown[0]?.querySelector(".coloride-agent-card-time")?.textContent).toBe("2.5s");
    // 2.2 - 1.0 = 1.2s here, so craft a really quick one:
    const quick = clocked();
    quick.at(0);
    quick.store.enqueue(call("q", "in_progress"));
    quick.at(300);
    quick.store.enqueue(call("q", "completed"));
    const [card] = new ConversationView().elements(
      document,
      quick.store.getState(),
      button,
      actions,
    );
    expect(card?.querySelector(".coloride-agent-card-time")).toBeNull();
  });
  it("tells how long a turn took and how much of it was waiting for the user", () => {
    const { store, at } = clocked();
    at(1000);
    store.enqueue({ type: "message", role: "user", text: "go" });
    store.enqueue({
      type: "update",
      update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "ok" } },
    });
    at(2000);
    store.enqueue({ type: "permission", id: "p", reason: "run", options: [] });
    at(5000);
    store.answered("p");
    at(9000);
    store.enqueue({ type: "update", update: { sessionUpdate: "turn_complete" } });
    expect(store.getState().turn).toEqual({ startedAt: 1000, endedAt: 9000, waitingMs: 3000 });
    const shown = new ConversationView().elements(document, store.getState(), button, actions);
    expect(shown.at(-1)?.textContent).toBe("Worked for 8.0s · 3.0s waiting for you");
    // Not while it is still going.
    const live = clocked();
    live.store.enqueue({ type: "message", role: "user", text: "go" });
    const during = new ConversationView().elements(
      document,
      live.store.getState(),
      button,
      actions,
    );
    expect(during.some((el) => el.classList.contains("coloride-agent-turn-time"))).toBe(false);
  });
});

describe("what the agent lets the user choose", () => {
  it("keeps only well-formed options, and drops them with the session", () => {
    const store = new AgentStore((callback) => callback());
    store.enqueue({
      type: "options",
      options: [
        model,
        { id: "x", name: "No choices", current: "a", choices: [] },
        { id: 5, name: "Bad id", current: "a", choices: [{ value: "a", name: "A" }] },
        "junk",
      ],
    });
    expect(store.getState().options.map((o) => o.id)).toEqual(["model"]);
    store.reconnect();
    expect(store.getState().options).toEqual([]);
  });
  it("shows the model first, picks only another value, and locks while a turn runs", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const picked = vi.fn();
    const bar = new OptionBar(document, host, picked);
    host.append(bar.element);
    bar.update([mode, model], false);
    const chips = [...bar.element.querySelectorAll("button")];
    expect(chips.map((c) => c.textContent)).toEqual(["Deep", "Default"]);
    chips[0]?.click();
    const items = [...host.querySelectorAll<HTMLButtonElement>(".sidebar-menu-item")];
    expect(items.map((i) => i.textContent)).toEqual(["Deep✓", "Fast"]);
    items[0]?.click();
    expect(picked).not.toHaveBeenCalled();
    chips[0]?.click();
    host.querySelectorAll<HTMLButtonElement>(".sidebar-menu-item")[1]?.click();
    expect(picked).toHaveBeenCalledWith("model", "fast");
    bar.update([mode, model], true);
    expect(bar.element.querySelector("button")?.disabled).toBe(true);
    bar.update([], false);
    expect(bar.element.hidden).toBe(true);
    host.remove();
  });
  it("sends the user's pick to the agent, and not while it is working", async () => {
    const setOption = vi.fn(async () => undefined);
    const emit: { send?: (event: never) => void } = {};
    const host: AgentHost = {
      start: vi.fn(async (_config, onEvent) => {
        emit.send = onEvent as never;
        return {
          setOption,
          prompt: vi.fn(),
          cancel: vi.fn(),
          stop: vi.fn(async () => undefined),
          answer: vi.fn(),
        };
      }),
    };
    const store = new AgentStore((callback) => callback());
    const panel = new AgentPanel(
      document,
      host,
      async () => undefined,
      { agentId: "codex", name: "Codex" },
      true,
      store,
    );
    document.body.append(panel.element);
    for (let i = 0; i < 10; i++) await Promise.resolve();
    store.enqueue({ type: "ready", sessionId: "s" });
    store.enqueue({ type: "options", options: [model, mode] });
    const chip = panel.element.querySelector<HTMLButtonElement>(".coloride-agent-option");
    expect(chip?.textContent).toBe("Deep");
    expect(chip?.disabled).toBe(false);
    chip?.click();
    panel.element.querySelectorAll<HTMLButtonElement>(".sidebar-menu-item")[1]?.click();
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(setOption).toHaveBeenCalledWith("model", "fast");
    store.enqueue({ type: "message", role: "user", text: "work" });
    expect(panel.element.querySelector<HTMLButtonElement>(".coloride-agent-option")?.disabled).toBe(
      true,
    );
    expect(panel.element.querySelector(".coloride-agent-busy-time")?.textContent).toMatch(/s$/);
    panel.destroy();
  });
});

describe("the thinking of the agent", () => {
  const thinking = (store: AgentStore, text: string) =>
    store.enqueue({
      type: "update",
      update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text } },
    });
  it("reads as formatted text without the blank lines it starts with", () => {
    const store = new AgentStore((callback) => callback());
    thinking(store, "\n\n**Building memory-hash comparison**\n\nThen compare `dist/`.");
    const [card] = new ConversationView().elements(document, store.getState(), button, actions);
    const body = card?.querySelector(".coloride-agent-card-body");
    expect(body?.querySelector("strong")?.textContent).toBe("Building memory-hash comparison");
    expect(body?.querySelector("code")?.textContent).toBe("dist/");
    expect(body?.textContent).not.toContain("**");
    expect(body?.innerHTML.startsWith("<p>")).toBe(true);
  });
  it("says briefly when it took no time, and the seconds when it did", () => {
    let now = 0;
    const store = new AgentStore(
      (callback) => callback(),
      () => now,
    );
    thinking(store, "quick");
    now = 300;
    store.enqueue({ type: "update", update: { sessionUpdate: "turn_complete" } });
    const view = new ConversationView();
    const label = () =>
      view
        .elements(document, store.getState(), button, actions)[0]
        ?.querySelector(".coloride-agent-card-verb")?.textContent;
    expect(label()).toBe("Thought briefly");
    const slow = new AgentStore(
      (callback) => callback(),
      () => now,
    );
    now = 0;
    thinking(slow, "long");
    now = 8000;
    slow.enqueue({ type: "update", update: { sessionUpdate: "turn_complete" } });
    const [card] = new ConversationView().elements(document, slow.getState(), button, actions);
    expect(card?.querySelector(".coloride-agent-card-verb")?.textContent).toBe("Thought for 8s");
  });
});

describe("the thinking block, as a reasoning component", () => {
  const part = (store: AgentStore, text: string) =>
    store.enqueue({
      type: "update",
      update: { sessionUpdate: "agent_thought_chunk", content: { type: "text", text } },
    });
  it("reads the parts of one reasoning as one block with a paragraph for each", () => {
    const store = new AgentStore((callback) => callback());
    part(store, "**First idea**\nDetails.");
    part(store, "**Second idea**\nMore.");
    part(store, " continues the same sentence");
    expect(store.getState().thoughts).toHaveLength(1);
    expect(store.getState().thoughts[0]?.text).toBe(
      "**First idea**\nDetails.\n\n**Second idea**\nMore. continues the same sentence",
    );
  });
  it("is open while it thinks, stays open a moment after, then folds by itself", () => {
    vi.useFakeTimers();
    const store = new AgentStore((callback) => callback());
    const view = new ConversationView();
    part(store, "thinking");
    const open = () =>
      view.elements(document, store.getState(), button, actions)[0]?.getAttribute("data-open");
    expect(open()).toBe("true");
    store.enqueue({ type: "update", update: { sessionUpdate: "turn_complete" } });
    expect(open()).toBe("true");
    vi.advanceTimersByTime(1100);
    expect(open()).toBe("false");
    view.dispose();
    vi.useRealTimers();
  });
  it("lets a click win over the automatic open and fold", () => {
    vi.useFakeTimers();
    const store = new AgentStore((callback) => callback());
    const view = new ConversationView();
    part(store, "thinking");
    const [card] = view.elements(document, store.getState(), button, actions);
    card?.querySelector("button")?.click();
    expect(card?.getAttribute("data-open")).toBe("false");
    store.enqueue({ type: "update", update: { sessionUpdate: "turn_complete" } });
    view.elements(document, store.getState(), button, actions);
    vi.advanceTimersByTime(1100);
    expect(card?.getAttribute("data-open")).toBe("false");
    view.dispose();
    vi.useRealTimers();
  });
});
