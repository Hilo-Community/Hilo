import { describe, expect, it, vi } from "vitest";
import { AGENT_DONE_SOUND, createAgentFinishedAlert } from "./agent-alert.js";

/** A fake `invoke` that answers by command name, recording every call in order. */
function fakeInvoke(responses: Record<string, unknown>) {
  const calls: Array<[string, Record<string, unknown> | undefined]> = [];
  const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    calls.push([command, args]);
    if (command in responses) return responses[command];
    throw new Error(`unexpected command: ${command}`);
  });
  return { invoke, calls };
}

describe("agent finished alert", () => {
  it("plays the sound from the start and shows a notification named after the session, once permission is already granted", async () => {
    const play = vi.fn(async () => undefined);
    const sound = { currentTime: 9, play };
    const createSound = vi.fn(() => sound);
    const { invoke, calls } = fakeInvoke({ "plugin:notification|is_permission_granted": true });
    const alert = createAgentFinishedAlert(invoke as never, createSound);

    alert({ chatId: "chat-1", title: "Codex", attended: false });
    expect(createSound).toHaveBeenCalledWith(AGENT_DONE_SOUND);
    expect(sound.currentTime).toBe(0);
    expect(play).toHaveBeenCalledOnce();
    await vi.waitFor(() =>
      expect(calls).toContainEqual(["plugin:notification|notify", expect.anything()]),
    );
    expect(calls).toEqual([
      ["plugin:notification|is_permission_granted", undefined],
      ["plugin:notification|notify", { options: { title: "Codex", body: "Finished" } }],
    ]);

    // The sound is created once and rewound for the next one; permission is checked only once.
    sound.currentTime = 1;
    alert({ chatId: "chat-1", title: "Codex", attended: false });
    expect(createSound).toHaveBeenCalledOnce();
    expect(sound.currentTime).toBe(0);
    expect(play).toHaveBeenCalledTimes(2);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledTimes(3));
    expect(
      calls.filter(([command]) => command === "plugin:notification|is_permission_granted"),
    ).toHaveLength(1);
  });

  it("requests permission once, the first time it is not already granted, and notifies once granted", async () => {
    const { invoke, calls } = fakeInvoke({
      "plugin:notification|is_permission_granted": false,
      "plugin:notification|request_permission": "granted",
    });
    const alert = createAgentFinishedAlert(invoke as never, () => ({
      currentTime: 0,
      play: async () => undefined,
    }));

    alert({ chatId: "chat-1", title: "Claude", attended: false });
    await vi.waitFor(() =>
      expect(calls).toContainEqual(["plugin:notification|notify", expect.anything()]),
    );
    expect(calls).toEqual([
      ["plugin:notification|is_permission_granted", undefined],
      ["plugin:notification|request_permission", undefined],
      ["plugin:notification|notify", { options: { title: "Claude", body: "Finished" } }],
    ]);
  });

  it("never calls notify when permission is refused", async () => {
    const { invoke, calls } = fakeInvoke({
      "plugin:notification|is_permission_granted": false,
      "plugin:notification|request_permission": "denied",
    });
    const alert = createAgentFinishedAlert(invoke as never, () => ({
      currentTime: 0,
      play: async () => undefined,
    }));

    alert({ chatId: "chat-1", title: "Claude", attended: false });
    await vi.waitFor(() =>
      expect(calls).toContainEqual(["plugin:notification|request_permission", undefined]),
    );
    await Promise.resolve();
    await Promise.resolve();
    expect(calls.some(([command]) => command === "plugin:notification|notify")).toBe(false);
  });

  it("plays the sound but shows no notification for the session the user is looking at", async () => {
    const play = vi.fn(async () => undefined);
    const { invoke } = fakeInvoke({ "plugin:notification|is_permission_granted": true });
    const alert = createAgentFinishedAlert(invoke as never, () => ({ currentTime: 0, play }));

    alert({ chatId: "chat-1", title: "Codex", attended: true });

    expect(play).toHaveBeenCalledOnce();
    await Promise.resolve();
    await Promise.resolve();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("does not fail the app when the sound, the permission check, or the notification is refused", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const alert = createAgentFinishedAlert(
      (async () => {
        throw new Error("denied");
      }) as never,
      () => ({
        currentTime: 0,
        play: async () => {
          throw new Error("blocked");
        },
      }),
    );
    expect(() => alert({ chatId: "chat-1", title: "Claude", attended: false })).not.toThrow();
    await vi.waitFor(() => expect(warn).toHaveBeenCalledOnce());
    warn.mockRestore();
  });
});
