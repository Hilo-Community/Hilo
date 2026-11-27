import { describe, expect, it } from "vitest";
import { JUDGE_EVENT, createTauriJudgeHost } from "./tauri-judge-host.js";

describe("Tauri Judge host (LOT-015)", () => {
  it("calls the Rust commands with their arguments and relays Judge events", async () => {
    const calls: Array<[string, Record<string, unknown> | undefined]> = [];
    const invoke = async <T>(command: string, args?: Record<string, unknown>) => {
      calls.push([command, args]);
      return [] as T;
    };
    let handler: ((event: { payload: unknown }) => void) | undefined;
    let listenedTo = "";
    const listen = async <T>(event: string, h: (event: { payload: T }) => void) => {
      listenedTo = event;
      handler = h as (event: { payload: unknown }) => void;
      return () => {};
    };
    const host = createTauriJudgeHost(invoke, listen);
    const received: unknown[] = [];
    await host.listen((event) => received.push(event));
    handler?.({ payload: { type: "verifying", agentId: 1, attempt: 1 } });
    expect(listenedTo).toBe(JUDGE_EVENT);
    expect(received).toEqual([{ type: "verifying", agentId: 1, attempt: 1 }]);

    await host.accept("r1", "src/a.ts", "src/a.ts", 3);
    await host.reject("r1");
    await host.adjust("r1", "keep the name");
    await host.documentStates("src/a.ts", 3);
    expect(calls).toEqual([
      [
        "judge_accept_command",
        { reviewId: "r1", path: "src/a.ts", documentId: "src/a.ts", sessionGeneration: 3 },
      ],
      ["judge_reject_command", { reviewId: "r1" }],
      ["judge_adjust_command", { reviewId: "r1", note: "keep the name" }],
      ["judge_document_states_command", { documentId: "src/a.ts", sessionGeneration: 3 }],
    ]);
  });
});
