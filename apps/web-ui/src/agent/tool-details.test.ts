import { describe, expect, it } from "vitest";
import { AgentStore } from "./store.js";
import {
  TOOL_DETAILS_BUDGET,
  TOOL_INPUT_LIMIT,
  TOOL_OUTPUT_LIMIT,
  detailsSize,
  toolDetails,
} from "./tool-details.js";

const event = (update: Record<string, unknown>) => ({
  type: "update" as const,
  update: { sessionUpdate: "tool_call_update", toolCallId: "cmd", ...update },
});
describe("bounded tool previews", () => {
  it("keeps commands, text output and real exit codes across partial updates", () => {
    const store = new AgentStore(() => {});
    store.enqueue(
      event({
        kind: "execute",
        rawInput: { command: "pnpm test", cwd: "src" },
        status: "in_progress",
      }),
    );
    store.enqueue(
      event({
        content: [{ type: "content", content: { type: "text", text: "passed" } }],
        rawOutput: { exitStatus: { exitCode: 0 } },
        status: "completed",
      }),
    );
    store.flush();
    expect(store.getState().tools[0]).toMatchObject({
      command: "pnpm test",
      output: "passed",
      exitCode: 0,
      status: "done",
    });
    store.enqueue(
      event({
        content: [{ type: "content", content: { type: "text", text: "replacement snapshot" } }],
      }),
    );
    store.flush();
    expect(store.getState().tools[0]?.output).toBe("replacement snapshot");
  });
  it("clips huge fields before retaining them and excludes binary and nested payloads", () => {
    const huge = "x".repeat(3 * 1024 * 1024);
    const details = toolDetails({
      rawInput: { command: huge, nested: { secret: huge } },
      content: [
        { type: "content", content: { type: "image", data: huge } },
        { type: "content", content: { type: "text", text: huge } },
      ],
    });
    expect(details.command).toHaveLength(TOOL_INPUT_LIMIT);
    expect(details.output).toHaveLength(TOOL_OUTPUT_LIMIT);
    expect(details.arguments?.map(([key]) => key)).toEqual(["command"]);
    expect(details.truncated).toBe(true);
  });
  it("bounds previews across tools and does not charge repeated snapshots to the session budget", () => {
    const store = new AgentStore(() => {});
    const huge = "x".repeat(3 * 1024 * 1024);
    for (let i = 0; i < 180; i++) {
      store.enqueue(
        event({
          kind: "execute",
          toolCallId: String(i % 20),
          rawOutput: { output: huge, exitCode: 0 },
          status: "completed",
        }),
      );
      store.flush();
    }
    expect(store.getState().error).toBe("");
    expect(
      store.getState().tools.reduce((sum, tool) => sum + detailsSize(tool), 0),
    ).toBeLessThanOrEqual(TOOL_DETAILS_BUDGET);
    expect(store.getState().tools.every((tool) => tool.status === "done")).toBe(true);
    expect(store.getState().tools.some((tool) => tool.truncated)).toBe(true);
    store.reset();
    store.enqueue(event({ rawOutput: { output: huge } }));
    store.flush();
    expect(store.getState().tools[0]?.output).toHaveLength(TOOL_OUTPUT_LIMIT);
  });
  it("tracks thought duration and completes it when the assistant replies", () => {
    let now = 0;
    const store = new AgentStore(
      () => {},
      () => now,
    );
    store.enqueue(
      event({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Checking " } }),
    );
    store.flush();
    now = 8000;
    store.enqueue(
      event({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "the code" } }),
    );
    store.enqueue(
      event({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Done" } }),
    );
    store.flush();
    expect(store.getState().thoughts[0]).toMatchObject({
      text: "Checking the code",
      running: false,
      durationMs: 8000,
    });
  });
});
