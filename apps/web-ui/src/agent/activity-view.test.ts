// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import { activityEntries } from "./activity-view.js";
import { ConversationView } from "./conversation-view.js";
import { AgentStore, type ToolCard } from "./store.js";
import { ToolCardView, toolLabel } from "./tool-card-view.js";

const tool = (id: string, kind: string, status: ToolCard["status"] = "done"): ToolCard => ({
  id,
  kind,
  status,
  title: `${kind} target`,
  locations: [],
});
const button = (label: string, click: () => void) => {
  const node = document.createElement("button");
  node.textContent = label;
  node.onclick = click;
  return node;
};
const actions = { openLocation: vi.fn(), answer: vi.fn() };
const timeline = (tools: ToolCard[]) => tools.map(({ id }) => ({ type: "tool" as const, id }));

function setup(tools: ToolCard[]) {
  const root = document.createElement("div");
  document.body.append(root);
  const view = new ConversationView();
  const state = { ...new AgentStore(() => {}).getState(), tools, timeline: timeline(tools) };
  const render = (next = state) => view.render(root, next, button, actions);
  render();
  return { root, state, render, view };
}

describe("exploration activities", () => {
  it("groups explicit exploration tools and keeps command/message/thought boundaries", () => {
    const tools = [
      tool("a", "read"),
      tool("b", "search"),
      tool("c", "execute"),
      tool("d", "read"),
      tool("e", "fetch"),
    ];
    const entries = activityEntries(
      [
        ...timeline(tools.slice(0, 3)),
        { type: "message", id: "m" },
        ...timeline(tools.slice(3, 4)),
        { type: "thought", id: "t" },
        ...timeline(tools.slice(4)),
      ],
      new Map(tools.map((tool) => [tool.id, tool])),
    );
    expect(entries.map((entry) => entry.type)).toEqual([
      "exploration",
      "tool",
      "message",
      "exploration",
      "thought",
      "exploration",
    ]);
    expect(entries[0]).toMatchObject({ id: "a", tools: [{ id: "a" }, { id: "b" }] });
    // An edit proposal keeps its own review card, even if an agent changes its reported kind.
    const edit = {
      ...tools[0],
      ...tool("edit", "read"),
      edit: { id: "edit", path: "a", baseContentHash: "h", oldText: "a", newText: "b" },
    };
    expect(activityEntries(timeline([edit]), new Map([[edit.id, edit]]))[0]?.type).toBe("tool");
  });
  it("shows accurate call counts, opens while active, and collapses when done", () => {
    const { root, state, render } = setup([tool("a", "read"), tool("b", "search", "running")]);
    const header = root.querySelector(".hilo-agent-exploration > button") as HTMLButtonElement;
    expect(header.textContent).toContain("Exploring…");
    expect(header.textContent).toContain("1 read, 1 search");
    expect(header.getAttribute("aria-expanded")).toBe("true");
    render({ ...state, tools: state.tools.map((t) => ({ ...t, status: "done" })) });
    expect(header.textContent).toContain("Explored");
    expect(header.getAttribute("aria-expanded")).toBe("false");
    expect(root.querySelector(".hilo-agent-exploration [role=list]")?.children).toHaveLength(2);
    root.remove();
  });
  it("preserves manual disclosure, nested tool focus and scroll as a group grows", () => {
    const { root, state, render } = setup([tool("a", "search", "running")]);
    const header = root.querySelector(".hilo-agent-exploration > button") as HTMLButtonElement;
    header.click();
    render({ ...state, tools: [tool("a", "search")] });
    expect(header.getAttribute("aria-expanded")).toBe("false");
    header.click();
    const nested = root.querySelector(".hilo-agent-tool > button") as HTMLButtonElement;
    // Search has a bounded output body to expand.
    render({ ...state, tools: [{ ...tool("a", "search"), output: "one\ntwo" }] });
    nested.focus();
    nested.click();
    const read = tool("b", "read");
    render({
      ...state,
      tools: [{ ...tool("a", "search"), output: "one\ntwo\nthree" }, read],
      timeline: [...state.timeline, ...timeline([read])],
    });
    expect(root.querySelector(".hilo-agent-exploration > button")).toBe(header);
    expect(root.querySelector(".hilo-agent-tool > button")).toBe(nested);
    expect(document.activeElement).toBe(nested);
    expect(nested.getAttribute("aria-expanded")).toBe("true");
    expect(header.getAttribute("aria-expanded")).toBe("true");
    root.remove();
  });
  it.each([
    ["failed", "Exploration failed"],
    ["denied", "Exploration stopped"],
    ["pending", "Waiting to explore"],
  ] as const)("retains the %s state and details", (status, label) => {
    const { root } = setup([tool("a", "read", status)]);
    const group = root.querySelector(".hilo-agent-exploration");
    expect(group?.getAttribute("data-status")).toBe(status);
    expect(group?.querySelector("button")?.getAttribute("aria-label")).toContain(label);
    expect(group?.querySelector("button")?.getAttribute("aria-expanded")).toBe("true");
    root.remove();
  });
  it("does not merge across messages and clears group disclosures on reset", () => {
    const { root, state, view } = setup([tool("a", "read"), tool("b", "search")]);
    view.render(
      root,
      {
        ...state,
        messages: [{ id: "m", role: "assistant", text: "Next", html: ["Next"] }],
        timeline: [
          { type: "tool", id: "a" },
          { type: "message", id: "m" },
          { type: "tool", id: "b" },
        ],
      },
      button,
      actions,
    );
    expect(root.children).toHaveLength(3);
    expect(root.children[1]?.textContent).toBe("Next");
    (root.querySelector("button") as HTMLButtonElement).click();
    view.render(root, { ...state, tools: [], timeline: [] }, button, actions);
    view.render(root, state, button, actions);
    expect(root.querySelector("button")?.getAttribute("aria-expanded")).toBe("false");
    root.remove();
  });
});

describe("ACP thinking tools", () => {
  it("uses Thinking/Thought with the real title and bounded summary, without Calling", () => {
    const view = new ToolCardView(document);
    const thinking = {
      ...tool("think", "think", "running"),
      title: "Guardian Review",
      output: "<script>Review the proposal</script>",
    };
    const element = view.update(thinking, vi.fn());
    expect(toolLabel(thinking)).toEqual({ verb: "Thinking…", target: "Guardian Review" });
    expect(element.classList.contains("hilo-agent-activity")).toBe(true);
    expect(element.textContent).not.toContain("Calling");
    expect(element.querySelector("script")).toBeNull();
    expect(view.card.header.getAttribute("aria-expanded")).toBe("true");
    view.update({ ...thinking, status: "done" }, vi.fn());
    expect(view.card.header.textContent).toContain("Thought");
    expect(view.card.header.getAttribute("aria-expanded")).toBe("false");
    view.card.header.click();
    view.update({ ...thinking, status: "done", output: "Final summary" }, vi.fn());
    expect(view.card.header.getAttribute("aria-expanded")).toBe("true");
  });
  it("disables an empty reasoning disclosure instead of inventing a summary or duration", () => {
    const view = new ToolCardView(document);
    view.update({ ...tool("think", "think"), title: "Review" }, vi.fn());
    expect(view.card.header.disabled).toBe(true);
    expect(view.card.header.textContent).not.toContain("for");
  });
});
