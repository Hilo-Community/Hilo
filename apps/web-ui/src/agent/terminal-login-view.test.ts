// @vitest-environment happy-dom
import { describe, expect, it, vi } from "vitest";
import type { AgentHandle } from "./store.js";
import { TerminalLoginView } from "./terminal-login-view.js";

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};
function fixture(startLogin = vi.fn(async () => {})) {
  const handle = {
    startLogin,
    resizeLogin: vi.fn(async () => {}),
    writeLogin: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
  } as unknown as AgentHandle;
  const view = new TerminalLoginView(
    document,
    () => handle,
    (action) => void action().catch(() => {}),
  );
  return { handle, view };
}
const visible = (view: TerminalLoginView) =>
  [...view.element.querySelectorAll("button")].filter((b) => !b.hidden).map((b) => b.textContent);

describe("terminal sign-in at the bottom", () => {
  it("starts by itself once, with no start button", async () => {
    const { handle, view } = fixture();
    view.render({ name: "Claude Subscription", args: ["--cli"], env: {} });
    await settle();
    expect(handle.startLogin).toHaveBeenCalledOnce();
    view.render({ name: "Claude Subscription", args: ["--cli"], env: {} });
    await settle();
    expect(handle.startLogin).toHaveBeenCalledOnce();
    expect(visible(view)).toEqual(["Open page again", "Cancel"]);
    expect(view.element.textContent).not.toContain("--cli");
  });

  it("asks first when the agent wants environment overrides", async () => {
    const { handle, view } = fixture();
    view.render({ name: "X", args: [], env: { A: "1" } });
    await settle();
    expect(handle.startLogin).not.toHaveBeenCalled();
    expect(view.element.textContent).toContain("Environment overrides");
    expect(visible(view)).toContain("Start sign-in");
  });

  it("does not retry a failed start by itself", async () => {
    const { handle, view } = fixture(vi.fn().mockRejectedValue(new Error("no")));
    view.render({ name: "X", args: [], env: {} });
    await settle();
    view.render({ name: "X", args: [], env: {} });
    await settle();
    expect(handle.startLogin).toHaveBeenCalledOnce();
    expect(visible(view)).toContain("Start sign-in");
  });

  it("hides when there is no request", () => {
    const { view } = fixture();
    view.render(null);
    expect(view.element.hidden).toBe(true);
  });
});
